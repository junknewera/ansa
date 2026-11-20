use super::{ISSUER, Result};
use crate::protocol::json::Json;
pub const DISCOVERY: &str = "https://auth.openai.com/.well-known/openid-configuration";
pub const AUTHORIZE: &str = "https://auth.openai.com/api/accounts/authorize";
pub const TOKEN: &str = "https://auth.openai.com/api/accounts/oauth/token";
pub const JWKS: &str = "https://auth.openai.com/.well-known/jwks.json";
pub const REVOKE: &str = "https://auth.openai.com/api/accounts/oauth/revoke";

pub fn discover(value: &Json) -> Result<()> {
    if value.text_field("issuer")? != ISSUER {
        return Err("auth_discovery_issuer");
    }
    for (field, expected) in [
        ("authorization_endpoint", AUTHORIZE),
        ("token_endpoint", TOKEN),
        ("jwks_uri", JWKS),
        ("revocation_endpoint", REVOKE),
    ] {
        if value.text_field(field)? != expected {
            return Err("auth_discovery_endpoints");
        }
    }
    Ok(())
}
