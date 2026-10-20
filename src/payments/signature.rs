use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

/// Default Stripe timestamp tolerance (seconds), same as Stripe's libraries.
pub const STRIPE_TOLERANCE_SECS: i64 = 300;

/// Verify a `Stripe-Signature` header ("t=<unix>,v1=<hex>[,v1=...]") over the raw body.
pub fn verify_stripe_signature(
    secret: &str,
    body: &[u8],
    header: Option<&str>,
    now_unix: i64,
    tolerance_secs: i64,
) -> bool {
    if secret.is_empty() {
        return false;
    }
    let Some(header) = header else { return false };

    let mut timestamp: Option<i64> = None;
    let mut signatures: Vec<Vec<u8>> = Vec::new();
    for part in header.split(',') {
        let mut kv = part.trim().splitn(2, '=');
        match (kv.next(), kv.next()) {
            (Some("t"), Some(v)) => timestamp = v.parse().ok(),
            (Some("v1"), Some(v)) => {
                if let Ok(bytes) = hex::decode(v) {
                    signatures.push(bytes);
                }
            }
            _ => {}
        }
    }
    let Some(t) = timestamp else { return false };
    if signatures.is_empty() || (now_unix - t).abs() > tolerance_secs {
        return false;
    }

    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(t.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    let expected = mac.finalize().into_bytes();
    signatures
        .iter()
        .any(|s| bool::from(expected.as_slice().ct_eq(s.as_slice())))
}

/// Constant-time token comparison. An empty expected token never matches.
pub fn token_matches(expected: &str, provided: &str) -> bool {
    !expected.is_empty() && bool::from(expected.as_bytes().ct_eq(provided.as_bytes()))
}

/// Compute a Stripe-style v1 signature (used by tests and local tooling).
pub fn stripe_v1_signature(secret: &str, timestamp: i64, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}