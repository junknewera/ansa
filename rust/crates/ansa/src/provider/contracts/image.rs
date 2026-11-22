//! Image bytes are owned at selection time and survive path removal and replay.
use super::Json;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::sync::Arc;

pub const MAX_IMAGES: usize = 4;
/// Aggregate original file bytes per submitted turn.
pub const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq)]
pub struct ImageInput {
    url: Arc<str>,
    digest: String,
    bytes: usize,
    mime: &'static str,
}
impl std::fmt::Debug for ImageInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageInput")
            .field("sha256", &self.digest)
            .field("mime", &self.mime)
            .field("bytes", &self.bytes)
            .finish()
    }
}

impl ImageInput {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, &'static str> {
        let mime = image_mime(bytes)?;
        Ok(Self {
            url: format!("data:{mime};base64,{}", STANDARD.encode(bytes)).into(),
            digest: crate::protocol::legacy_json_v1::sha256(bytes),
            bytes: bytes.len(),
            mime,
        })
    }
    pub fn from_data_url(url: &str) -> Result<Self, &'static str> {
        let (header, encoded) = url.split_once(',').ok_or("image_data_url")?;
        if !matches!(header, "data:image/png;base64" | "data:image/jpeg;base64")
            || encoded.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4
        {
            return Err("image_data_url");
        }
        let bytes = STANDARD.decode(encoded).map_err(|_| "image_base64")?;
        let mime = image_mime(&bytes)?;
        if header
            .strip_prefix("data:")
            .and_then(|s| s.strip_suffix(";base64"))
            != Some(mime)
        {
            return Err("image_data_url");
        }
        Ok(Self {
            url: url.into(),
            digest: crate::protocol::legacy_json_v1::sha256(&bytes),
            bytes: bytes.len(),
            mime,
        })
    }
    pub fn sha256(&self) -> &str {
        &self.digest
    }
    pub fn mime(&self) -> &str {
        self.mime
    }
    pub fn byte_len(&self) -> usize {
        self.bytes
    }
}

fn image_mime(bytes: &[u8]) -> Result<&'static str, &'static str> {
    if bytes.len() > MAX_IMAGE_BYTES || bytes.is_empty() {
        return Err("image_size");
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        crate::tools::artifacts::check_blob("image/png", bytes)?;
        return Ok("image/png");
    }
    if !bytes.starts_with(&[255, 216]) || !bytes.ends_with(&[255, 217]) {
        return Err("image_format");
    }
    // Walk JPEG metadata up to the first scan. Check segment lengths, a bounded
    // frame and presence of image data; the provider decodes the entropy stream.
    let mut rest = &bytes[2..bytes.len() - 2];
    let mut frame = false;
    while rest.starts_with(&[255]) {
        while rest.first() == Some(&255) {
            rest = &rest[1..];
        }
        let (&marker, segment) = rest.split_first().ok_or("image_jpeg")?;
        let size =
            u16::from_be_bytes(segment.get(..2).ok_or("image_jpeg")?.try_into().unwrap()) as usize;
        if size < 2 || size > segment.len() {
            return Err("image_jpeg");
        }
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            if size < 11 {
                return Err("image_jpeg");
            }
            let h = u32::from(u16::from_be_bytes([segment[3], segment[4]]));
            let w = u32::from(u16::from_be_bytes([segment[5], segment[6]]));
            if w == 0 || h == 0 || w * h > 8_000_000 || size != 8 + 3 * usize::from(segment[7]) {
                return Err("image_jpeg_dimensions");
            }
            frame = true;
        }
        if marker == 0xda {
            return if frame
                && size >= 8
                && size == 6 + 2 * usize::from(segment[2])
                && size < segment.len()
            {
                Ok("image/jpeg")
            } else {
                Err("image_jpeg_scan")
            };
        }
        rest = &segment[size..];
    }
    Err("image_jpeg")
}

pub(crate) fn validate_images(images: &[ImageInput]) -> Result<(), &'static str> {
    if images.is_empty()
        || images.len() > MAX_IMAGES
        || images.iter().map(ImageInput::byte_len).sum::<usize>() > MAX_IMAGE_BYTES
    {
        return Err("image_submit_limit");
    }
    Ok(())
}

pub(crate) fn content(text: &str, images: &[ImageInput]) -> Result<Json, &'static str> {
    if images.is_empty() {
        return Json::string(text);
    }
    validate_images(images)?;
    let mut parts = vec![Json::object(vec![
        ("type", Json::string("input_text")?),
        ("text", Json::string(text)?),
    ])?];
    for image in images {
        parts.push(Json::object(vec![
            ("type", Json::string("input_image")?),
            ("image_url", Json::string(&image.url)?),
        ])?);
    }
    Json::array(parts)
}

/// Validate the canonical user content against the independent message identity
/// in an accepted journal record. Text-only v1 records keep their original shape.
pub(crate) fn verify_content(
    event: &Json,
    text: &str,
    content: &Json,
) -> Result<Vec<String>, &'static str> {
    if !event.has("image_sha256") {
        return if content.text() == Ok(text) {
            Ok(Vec::new())
        } else {
            Err("session_request_conflict")
        };
    }
    if event.has("handoff") || event.has("draft") || event.has("orchestration_chat") {
        return Err("image_submit_mode");
    }
    let hashes = event.field("image_sha256")?.elements()?;
    let (caption, verified) = validate_content(content)?;
    if caption != text
        || hashes.len() != verified.len()
        || !hashes
            .iter()
            .zip(&verified)
            .all(|(hash, value)| hash.text() == Ok(value.as_str()))
    {
        return Err("image_submit_identity");
    }
    Ok(verified)
}

pub(crate) fn has_images(content: &Json) -> bool {
    content.elements().is_ok_and(|parts| {
        parts
            .iter()
            .any(|part| part.text_field("type") == Ok("input_image"))
    })
}

/// Fork seeds contain previously admitted user items without separate acceptance records.
pub(crate) fn validate_content(content: &Json) -> Result<(&str, Vec<String>), &'static str> {
    let (caption, images) = content
        .elements()?
        .split_first()
        .ok_or("image_submit_content")?;
    if images.is_empty()
        || images.len() > MAX_IMAGES
        || caption.keys()?.len() != 2
        || caption.text_field("type")? != "input_text"
    {
        return Err("image_submit_content");
    }
    let text = caption.text_field("text")?;
    let mut total = 0;
    let mut verified = Vec::with_capacity(images.len());
    for part in images {
        if part.keys()?.len() != 2 || part.text_field("type")? != "input_image" {
            return Err("image_submit_content");
        }
        let image = ImageInput::from_data_url(part.text_field("image_url")?)?;
        total += image.byte_len();
        if total > MAX_IMAGE_BYTES {
            return Err("image_submit_limit");
        }
        verified.push(image.digest);
    }
    Ok((text, verified))
}

#[cfg(test)]
mod tests {
    use super::*;
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAAAAAA6fptVAAAACklEQVR4nGNgAAAAAgABSK+kcQAAAABJRU5ErkJggg==";

    #[test]
    fn image_input_binds_bytes_mime_and_ordered_hashes() {
        let bytes = STANDARD.decode(PNG).unwrap();
        let image = ImageInput::from_bytes(&bytes).unwrap();
        let url = format!("data:image/png;base64,{PNG}");
        assert_eq!(image, ImageInput::from_data_url(&url).unwrap());
        assert!(ImageInput::from_data_url(&url.replace("image/png", "image/jpeg")).is_err());
        let hashes = Json::array(vec![Json::string(image.sha256()).unwrap()]).unwrap();
        let event = Json::object(vec![("image_sha256", hashes)]).unwrap();
        let parts = content("inspect", std::slice::from_ref(&image)).unwrap();
        assert_eq!(
            verify_content(&event, "inspect", &parts).unwrap(),
            [image.sha256()]
        );
        assert!(verify_content(&event, "different", &parts).is_err());
        assert!(
            verify_content(
                &event
                    .with("image_sha256", Json::parse(r#"["wrong"]"#).unwrap())
                    .unwrap(),
                "inspect",
                &parts
            )
            .is_err()
        );
        assert!(ImageInput::from_bytes(&bytes[..bytes.len() - 1]).is_err());
        assert!(validate_images(&vec![image; MAX_IMAGES + 1]).is_err());
    }
}
