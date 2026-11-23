//! Image bytes are owned at selection time and survive path removal and replay.
use super::Json;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, imageops::FilterType};
use std::{io::Cursor, sync::Arc};

pub const MAX_IMAGES: usize = 4;
/// Aggregate image bytes sent to the provider per submitted turn.
pub const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
/// Raw originals live with the accepted request in the 32 MiB journal record.
/// Their Base64 bytes and the maximum request frame must both fit there.
pub(crate) const MAX_ORIGINAL_IMAGE_BYTES: usize = Json::FRAME_BYTES / 2;
const MAX_PIXELS: u64 = 8_000_000;
const MIN_SCREENSHOT_EDGE: u32 = 1024;

#[derive(Clone, PartialEq, Eq)]
struct OriginalImage {
    url: Arc<str>,
    digest: String,
    bytes: usize,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ImageInput {
    url: Arc<str>,
    digest: String,
    bytes: usize,
    mime: &'static str,
    original: Option<OriginalImage>,
}
impl std::fmt::Debug for ImageInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageInput")
            .field("sha256", &self.sha256())
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
            original: None,
        })
    }

    pub(crate) fn fit(bytes: &[u8], target: usize) -> Result<Self, &'static str> {
        let target = target.min(MAX_IMAGE_BYTES);
        if bytes.len() <= target {
            return Self::from_bytes(bytes);
        }
        let mime = image_mime_with_limit(bytes, MAX_ORIGINAL_IMAGE_BYTES)?;
        if target == 0 {
            return Err("image_batch_bytes");
        }
        let format = if mime == "image/png" {
            ImageFormat::Png
        } else {
            ImageFormat::Jpeg
        };
        let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
        let mut limits = image::Limits::default();
        limits.max_alloc = Some(MAX_PIXELS * 8);
        reader.limits(limits);
        let mut decoder = reader.into_decoder().map_err(|_| "image_decode")?;
        let (width, height) = decoder.dimensions();
        if u64::from(width) * u64::from(height) > MAX_PIXELS {
            return Err("image_dimensions");
        }
        let orientation = decoder.orientation().map_err(|_| "image_decode")?;
        let mut picture = DynamicImage::from_decoder(decoder).map_err(|_| "image_decode")?;
        picture.apply_orientation(orientation);
        let minimum = picture
            .width()
            .max(picture.height())
            .min(MIN_SCREENSHOT_EDGE);
        loop {
            let mut encoded = Cursor::new(Vec::new());
            picture
                .write_to(&mut encoded, format)
                .map_err(|_| "image_encode")?;
            if encoded.get_ref().len() <= target {
                let mut image = Self::from_bytes(encoded.get_ref())?;
                image.original = Some(OriginalImage {
                    url: format!("data:{mime};base64,{}", STANDARD.encode(bytes)).into(),
                    digest: crate::protocol::legacy_json_v1::sha256(bytes),
                    bytes: bytes.len(),
                });
                return Ok(image);
            }
            let edge = picture.width().max(picture.height());
            if edge == minimum {
                return Err("image_batch_bytes");
            }
            let edge = (edge * 3 / 4).max(minimum);
            picture = picture.resize(edge, edge, FilterType::Triangle);
        }
    }

    pub fn from_data_url(url: &str) -> Result<Self, &'static str> {
        let (bytes, mime) = decode_url(url, MAX_IMAGE_BYTES)?;
        Ok(Self {
            url: url.into(),
            digest: crate::protocol::legacy_json_v1::sha256(&bytes),
            bytes: bytes.len(),
            mime,
            original: None,
        })
    }
    pub fn sha256(&self) -> &str {
        self.original
            .as_ref()
            .map_or(&self.digest, |original| &original.digest)
    }
    pub fn mime(&self) -> &str {
        self.mime
    }
    pub fn byte_len(&self) -> usize {
        self.bytes
    }
    pub(crate) fn original_byte_len(&self) -> usize {
        self.original
            .as_ref()
            .map_or(self.bytes, |image| image.bytes)
    }
}

fn decode_url(url: &str, limit: usize) -> Result<(Vec<u8>, &'static str), &'static str> {
    let (header, encoded) = url.split_once(',').ok_or("image_data_url")?;
    if !matches!(header, "data:image/png;base64" | "data:image/jpeg;base64")
        || encoded.len() > limit.div_ceil(3) * 4
    {
        return Err("image_data_url");
    }
    let bytes = STANDARD.decode(encoded).map_err(|_| "image_base64")?;
    let mime = image_mime_with_limit(&bytes, limit)?;
    if header
        .strip_prefix("data:")
        .and_then(|s| s.strip_suffix(";base64"))
        != Some(mime)
    {
        return Err("image_data_url");
    }
    Ok((bytes, mime))
}

fn image_mime(bytes: &[u8]) -> Result<&'static str, &'static str> {
    image_mime_with_limit(bytes, MAX_IMAGE_BYTES)
}

fn image_mime_with_limit(bytes: &[u8], limit: usize) -> Result<&'static str, &'static str> {
    if bytes.len() > limit || bytes.is_empty() {
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
            if w == 0
                || h == 0
                || u64::from(w * h) > MAX_PIXELS
                || size != 8 + 3 * usize::from(segment[7])
            {
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
        || images
            .iter()
            .map(ImageInput::original_byte_len)
            .sum::<usize>()
            > MAX_ORIGINAL_IMAGE_BYTES
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

pub(crate) fn originals(images: &[ImageInput]) -> Result<Option<Json>, &'static str> {
    if !images.iter().any(|image| image.original.is_some()) {
        return Ok(None);
    }
    Json::array(
        images
            .iter()
            .map(|image| match &image.original {
                Some(original) => Json::object(vec![
                    ("url", Json::string(&original.url)?),
                    ("sent_sha256", Json::string(&image.digest)?),
                ]),
                None => Json::parse("null"),
            })
            .collect::<Result<Vec<_>, _>>()?,
    )
    .map(Some)
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
    let (caption, images) = validate_content(content)?;
    let mut verified: Vec<_> = images.iter().map(|image| image.digest.clone()).collect();
    if event.has("image_originals") {
        let originals = event.field("image_originals")?.elements()?;
        if originals.len() != verified.len() || originals.iter().all(Json::is_null) {
            return Err("image_submit_identity");
        }
        let mut total = 0;
        for ((entry, image), digest) in originals.iter().zip(&images).zip(&mut verified) {
            if entry.is_null() {
                total += image.byte_len();
                continue;
            }
            let (original, mime) = decode_url(entry.text_field("url")?, MAX_ORIGINAL_IMAGE_BYTES)?;
            if entry.keys()?.len() != 2
                || entry.text_field("sent_sha256")? != digest
                || image.mime() != mime
            {
                return Err("image_submit_identity");
            }
            total += original.len();
            *digest = crate::protocol::legacy_json_v1::sha256(&original);
        }
        if total > MAX_ORIGINAL_IMAGE_BYTES {
            return Err("image_submit_limit");
        }
    }
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

// This substitution is also used to verify the archived image indexes saved
// in accepted journal records.
pub(crate) fn without_archived_images(item: &Json) -> Result<Option<Json>, &'static str> {
    if item.text_field("role") != Ok("user") {
        return Ok(None);
    }
    let Some((caption, images)) = item
        .field("content")
        .and_then(Json::elements)
        .ok()
        .and_then(<[Json]>::split_first)
    else {
        return Ok(None);
    };
    if caption.text_field("type") != Ok("input_text")
        || images.is_empty()
        || !images
            .iter()
            .all(|part| part.text_field("type") == Ok("input_image"))
    {
        return Ok(None);
    }
    let caption = caption.text_field("text")?;
    item.with(
        "content",
        Json::string(&format!(
            "{caption}\n[{} earlier image attachment(s) archived from model context.]",
            images.len()
        ))?,
    )
    .map(Some)
}

/// Fork seeds contain previously admitted user items without separate acceptance records.
pub(crate) fn validate_content(content: &Json) -> Result<(&str, Vec<ImageInput>), &'static str> {
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
        verified.push(image);
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

    #[test]
    fn resized_image_keeps_original_and_binds_the_model_copy() {
        let mut input = Cursor::new(Vec::new());
        let picture = DynamicImage::new_rgb8(1280, 960);
        for format in [ImageFormat::Png, ImageFormat::Jpeg] {
            input.get_mut().clear();
            input.set_position(0);
            picture.write_to(&mut input, format).unwrap();
            let bytes = input.get_ref();
            assert_eq!(ImageInput::fit(bytes, 128), Err("image_batch_bytes"));
            let image = ImageInput::fit(bytes, bytes.len() - 1).unwrap();
            assert!(image.byte_len() < bytes.len());
            assert_eq!(image.original_byte_len(), bytes.len());
            assert_eq!(
                image.sha256(),
                crate::protocol::legacy_json_v1::sha256(bytes)
            );
            let selected = [image];
            let saved = originals(&selected).unwrap().unwrap();
            assert_eq!(
                decode_url(
                    saved.elements().unwrap()[0].text_field("url").unwrap(),
                    MAX_ORIGINAL_IMAGE_BYTES
                )
                .unwrap()
                .0,
                *bytes
            );
            let event = Json::object(vec![
                (
                    "image_sha256",
                    Json::array(vec![Json::string(selected[0].sha256()).unwrap()]).unwrap(),
                ),
                ("image_originals", saved.clone()),
            ])
            .unwrap();
            let parts = content("inspect", &selected).unwrap();
            assert_eq!(
                verify_content(&event, "inspect", &parts).unwrap(),
                [selected[0].sha256()]
            );
            let changed = saved.elements().unwrap()[0]
                .with("sent_sha256", Json::string("different").unwrap())
                .unwrap();
            let changed = event
                .with("image_originals", Json::array(vec![changed]).unwrap())
                .unwrap();
            assert!(verify_content(&changed, "inspect", &parts).is_err());
        }
    }
}
