use super::Result;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::rand::{SecureRandom, SystemRandom};

pub fn base64url(bytes: &[u8]) -> Result<String> {
    if bytes.len() > 1024 * 1024 {
        return Err("auth_encoding_limit");
    }
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
pub fn unbase64url(text: &str) -> Result<Vec<u8>> {
    if text.is_empty()
        || text.len() > 65536
        || text.len() % 4 == 1
        || !text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("auth_encoding");
    }
    let bytes = URL_SAFE_NO_PAD.decode(text).map_err(|_| "auth_encoding")?;
    if URL_SAFE_NO_PAD.encode(&bytes) != text {
        return Err("auth_encoding");
    }
    Ok(bytes)
}
pub fn random_token(bytes: usize) -> Result<String> {
    if !(16..=64).contains(&bytes) {
        return Err("auth_random_size");
    }
    let mut value = vec![0; bytes];
    SystemRandom::new()
        .fill(&mut value)
        .map_err(|_| "auth_random")?;
    base64url(&value)
}
pub fn challenge(verifier: &str) -> String {
    // The legacy C++ hashing helper is total over strings. oauth2 asserts its
    // RFC length precondition, so keep that assertion away from untrusted
    // compatibility inputs. Login always generates a 43-byte verifier.
    if !(43..=128).contains(&verifier.len()) {
        return URL_SAFE_NO_PAD
            .encode(ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes()).as_ref());
    }
    oauth2::PkceCodeChallenge::from_code_verifier_sha256(&oauth2::PkceCodeVerifier::new(
        verifier.to_owned(),
    ))
    .as_str()
    .to_owned()
}
pub fn form_encode(fields: &[(&str, &str)]) -> String {
    fn encode(text: &str) -> String {
        let mut out = String::new();
        for byte in text.bytes() {
            if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
                out.push(byte as char);
            } else {
                use std::fmt::Write;
                write!(out, "%{byte:02X}").expect("String write");
            }
        }
        out
    }
    fields
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

pub fn host_id() -> Result<String> {
    let mut bytes = unbase64url(&random_token(16)?)?;
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    let mut out = String::from("urn:uuid:");
    for (i, byte) in bytes.iter().enumerate() {
        if [4, 6, 8, 10].contains(&i) {
            out.push('-');
        }
        use std::fmt::Write;
        write!(out, "{byte:02x}").expect("String write");
    }
    Ok(out)
}
