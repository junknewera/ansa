use super::{ISSUER, Result, pkce::unbase64url};
use crate::protocol::json::Json;
use std::collections::HashSet;

pub fn verify_id_token(
    jwt: &str,
    jwks: &Json,
    client: &str,
    nonce: &str,
    now: u64,
    refresh: bool,
) -> Result<Json> {
    if jwt.len() > 32768 {
        return Err("auth_jwt_limit");
    }
    let parts: Vec<_> = jwt.split('.').collect();
    if parts.len() != 3 {
        return Err("auth_jwt_shape");
    }
    let parse = |encoded: &str, bytes, depth, nodes| {
        let decoded = unbase64url(encoded)?;
        Json::parse_with_limits(
            std::str::from_utf8(&decoded).map_err(|_| "auth_encoding")?,
            bytes,
            depth,
            nodes,
        )
    };
    let header = parse(parts[0], 4096, 8, 64)?;
    if header.text_field("alg")? != "RS256" || ["crit", "jku", "x5u"].iter().any(|k| header.has(k))
    {
        return Err("auth_jwt_algorithm");
    }
    let kid = header.text_field("kid")?;
    if kid.is_empty() || kid.len() > 256 {
        return Err("auth_jwt_kid");
    }
    let keys = jwks.field("keys")?.elements()?;
    if keys.len() > 64 {
        return Err("auth_jwks_limit");
    }
    let mut selected = None;
    for key in keys {
        if key.has("kid") && key.text_field("kid")? == kid && selected.replace(key).is_some() {
            return Err("auth_duplicate_key");
        }
    }
    let key = selected.ok_or("auth_unknown_key")?;
    if key.text_field("kty")? != "RSA"
        || (key.has("alg") && key.text_field("alg")? != "RS256")
        || (key.has("use") && key.text_field("use")? != "sig")
    {
        return Err("auth_key_type");
    }
    if key.has("key_ops") && key.field("key_ops")? != &Json::parse("[\"verify\"]")? {
        return Err("auth_key_usage");
    }
    let n = unbase64url(key.text_field("n")?)?;
    let e = unbase64url(key.text_field("e")?)?;
    // ring rounds its minimum RSA bit length to bytes. Enforce the exact C++
    // 2048-bit minimum ourselves; canonical components and <=33-bit exponent
    // are explicit stricter ring constraints, covered by differential cases.
    if !(256..=1024).contains(&n.len()) || n[0] == 0 || e.is_empty() || e.len() > 5 || e[0] == 0 {
        return Err("auth_key_size");
    }
    let bits = n.len() * 8 - n[0].leading_zeros() as usize;
    let exponent = e.iter().fold(0u64, |v, b| (v << 8) | u64::from(*b));
    if !(2048..=8192).contains(&bits) || !(3..(1u64 << 33)).contains(&exponent) || exponent & 1 == 0
    {
        return Err("auth_key_size");
    }
    let signature = unbase64url(parts[2])?;
    let signed_length = parts[0].len() + 1 + parts[1].len();
    ring::signature::RsaPublicKeyComponents { n: &n, e: &e }
        .verify(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            &jwt.as_bytes()[..signed_length],
            &signature,
        )
        .map_err(|_| "auth_jwt_signature")?;
    let claims = parse(parts[1], 16384, 16, 1024)?;
    let subject = claims.text_field("sub")?;
    if claims.text_field("iss")? != ISSUER || subject.is_empty() || subject.len() > 256 {
        return Err("auth_jwt_identity");
    }
    let aud = claims.field("aud")?;
    if aud.is_string() {
        if aud.text()? != client {
            return Err("auth_jwt_audience");
        }
    } else {
        let values = aud.elements()?;
        if values.is_empty() || values.len() > 8 {
            return Err("auth_jwt_audience");
        }
        let mut unique = HashSet::new();
        for value in values {
            if !unique.insert(value.text()?) {
                return Err("auth_jwt_audience");
            }
        }
        if !unique.contains(client)
            || (unique.len() > 1 && (!claims.has("azp") || claims.text_field("azp")? != client))
        {
            return Err("auth_jwt_audience");
        }
    }
    if claims.has("azp") && claims.text_field("azp")? != client {
        return Err("auth_jwt_audience");
    }
    let exp = claims.usize_field("exp")? as u64;
    let iat = claims.usize_field("iat")? as u64;
    let skew = now.checked_add(60).ok_or("auth_jwt_time")?;
    if exp <= now
        || iat > skew
        || exp <= iat
        || (claims.has("nbf") && claims.usize_field("nbf")? as u64 > skew)
    {
        return Err("auth_jwt_time");
    }
    if !(refresh && !claims.has("nonce")) && claims.text_field("nonce")? != nonce {
        return Err("auth_jwt_nonce");
    }
    Ok(claims)
}
