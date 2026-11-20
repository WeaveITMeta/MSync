//! Compact JWS (RFC 7515) with ES256, enough for ID tokens and access tokens.

use base64::Engine;
use serde_json::{json, Value};

use super::keys::{KeyError, PublicKey, SigningKey, ALG};

const B64URL: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Sign `claims` with `key`. `typ` is `JWT` for ID tokens and `at+jwt` for access tokens.
pub fn sign(key: &SigningKey, typ: &str, claims: &Value) -> Result<String, KeyError> {
    let header = json!({"alg": ALG, "typ": typ, "kid": key.kid});
    let input = format!(
        "{}.{}",
        B64URL.encode(header.to_string()),
        B64URL.encode(claims.to_string())
    );
    let sig = key.sign(input.as_bytes())?;
    Ok(format!("{input}.{}", B64URL.encode(sig)))
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JwtError {
    #[error("malformed token")]
    Malformed,
    #[error("unsupported algorithm")]
    Alg,
    #[error("unknown key id")]
    UnknownKey,
    #[error("bad signature")]
    Signature,
    #[error("wrong token type")]
    Type,
    #[error("wrong issuer")]
    Issuer,
    #[error("wrong audience")]
    Audience,
    #[error("expired")]
    Expired,
}

/// The header and claims of a token whose ES256 signature verifies against one of `keys`.
pub fn verify(token: &str, keys: &[PublicKey]) -> Result<(Value, Value), JwtError> {
    let mut parts = token.split('.');
    let (Some(h), Some(c), Some(s), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(JwtError::Malformed);
    };
    let header: Value = serde_json::from_slice(&B64URL.decode(h).map_err(|_| JwtError::Malformed)?)
        .map_err(|_| JwtError::Malformed)?;
    if header.get("alg").and_then(Value::as_str) != Some(ALG) {
        return Err(JwtError::Alg);
    }
    let kid = header
        .get("kid")
        .and_then(Value::as_str)
        .ok_or(JwtError::UnknownKey)?;
    let key = keys
        .iter()
        .find(|k| k.kid == kid)
        .ok_or(JwtError::UnknownKey)?;
    let sig = B64URL.decode(s).map_err(|_| JwtError::Malformed)?;
    if !key.verify(format!("{h}.{c}").as_bytes(), &sig)
        && !crate::accounts::mutant::on("jwt_skip_signature")
    {
        return Err(JwtError::Signature);
    }
    let claims: Value = serde_json::from_slice(&B64URL.decode(c).map_err(|_| JwtError::Malformed)?)
        .map_err(|_| JwtError::Malformed)?;
    Ok((header, claims))
}

/// [`verify`], then the token type, issuer, audience (when given) and expiry.
pub fn verify_claims(
    token: &str,
    keys: &[PublicKey],
    typ: &str,
    iss: &str,
    aud: Option<&str>,
    now: i64,
) -> Result<Value, JwtError> {
    let (header, claims) = verify(token, keys)?;
    if header.get("typ").and_then(Value::as_str) != Some(typ) {
        return Err(JwtError::Type);
    }
    if claims.get("iss").and_then(Value::as_str) != Some(iss) {
        return Err(JwtError::Issuer);
    }
    if let Some(aud) = aud {
        if claims.get("aud").and_then(Value::as_str) != Some(aud) {
            return Err(JwtError::Audience);
        }
    }
    match claims.get("exp").and_then(Value::as_i64) {
        Some(exp) if exp > now => Ok(claims),
        _ => Err(JwtError::Expired),
    }
}
