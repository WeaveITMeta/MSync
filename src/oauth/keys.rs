//! Signing keys: ES256 (ECDSA P-256 with SHA-256), generated and used through `ring`.
//!
//! The private key is stored as PKCS#8 sealed with AES-256-GCM under
//! `MSYNC_KEY_ENCRYPTION_KEY` (32 bytes, as 64 hex characters or base64), with the key id as
//! the associated data so a sealed key cannot be moved to another key id. The newest unretired
//! key signs. Rotation retires the current key but keeps it in the JWKS until every token it
//! signed has expired.

use base64::Engine;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{EcdsaKeyPair, KeyPair, UnparsedPublicKey, ECDSA_P256_SHA256_FIXED, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde_json::{json, Value};
use sqlx::SqlitePool;

const B64URL: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

pub const ALG: &str = "ES256";

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("MSYNC_KEY_ENCRYPTION_KEY is not set or is not 32 bytes (64 hex characters or base64)")]
    NoKek,
    #[error("a stored signing key could not be opened (wrong MSYNC_KEY_ENCRYPTION_KEY?)")]
    Unseal,
    #[error("key generation or signing failed")]
    Crypto,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// The key-encryption key.
#[derive(Clone)]
pub struct Kek([u8; 32]);

impl std::fmt::Debug for Kek {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Kek(..)")
    }
}

impl Kek {
    /// 64 hex characters, or base64 (standard or URL-safe) of 32 bytes.
    pub fn parse(s: &str) -> Option<Kek> {
        let s = s.trim();
        let bytes = if s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
            hex::decode(s).ok()?
        } else {
            B64.decode(s).or_else(|_| B64URL.decode(s.trim_end_matches('='))).ok()?
        };
        <[u8; 32]>::try_from(bytes.as_slice()).ok().map(Kek)
    }

    pub fn from_bytes(b: [u8; 32]) -> Kek {
        Kek(b)
    }

    fn key(&self) -> LessSafeKey {
        LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &self.0).expect("32-byte AES key"))
    }

    pub fn seal(&self, kid: &str, plaintext: &[u8]) -> Result<String, KeyError> {
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new().fill(&mut nonce).map_err(|_| KeyError::Crypto)?;
        let mut buf = plaintext.to_vec();
        self.key()
            .seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(kid.as_bytes()), &mut buf)
            .map_err(|_| KeyError::Crypto)?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&buf);
        Ok(B64.encode(out))
    }

    pub fn open(&self, kid: &str, sealed: &str) -> Result<Vec<u8>, KeyError> {
        let raw = B64.decode(sealed).map_err(|_| KeyError::Unseal)?;
        if raw.len() < NONCE_LEN + 16 {
            return Err(KeyError::Unseal);
        }
        let (n, ct) = raw.split_at(NONCE_LEN);
        let nonce = Nonce::assume_unique_for_key(<[u8; NONCE_LEN]>::try_from(n).map_err(|_| KeyError::Unseal)?);
        let mut buf = ct.to_vec();
        let plain = self.key().open_in_place(nonce, Aad::from(kid.as_bytes()), &mut buf).map_err(|_| KeyError::Unseal)?;
        Ok(plain.to_vec())
    }
}

/// A signing key ready to use.
pub struct SigningKey {
    pub kid: String,
    pair: EcdsaKeyPair,
}

impl SigningKey {
    pub fn sign(&self, msg: &[u8]) -> Result<Vec<u8>, KeyError> {
        self.pair.sign(&SystemRandom::new(), msg).map(|s| s.as_ref().to_vec()).map_err(|_| KeyError::Crypto)
    }
}

/// A public key as published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey {
    pub kid: String,
    /// Uncompressed SEC1 point (0x04 || X || Y).
    pub point: Vec<u8>,
}

impl PublicKey {
    pub fn jwk(&self) -> Value {
        let (x, y) = if self.point.len() == 65 { (&self.point[1..33], &self.point[33..65]) } else { (&[][..], &[][..]) };
        json!({
            "kty": "EC",
            "crv": "P-256",
            "x": B64URL.encode(x),
            "y": B64URL.encode(y),
            "kid": self.kid,
            "alg": ALG,
            "use": "sig",
        })
    }

    /// Rebuild from a JWK (`kty` EC, `crv` P-256).
    pub fn from_jwk(j: &Value) -> Option<PublicKey> {
        if j.get("kty")?.as_str()? != "EC" || j.get("crv")?.as_str()? != "P-256" {
            return None;
        }
        let x = B64URL.decode(j.get("x")?.as_str()?).ok()?;
        let y = B64URL.decode(j.get("y")?.as_str()?).ok()?;
        if x.len() != 32 || y.len() != 32 {
            return None;
        }
        let mut point = vec![4u8];
        point.extend(x);
        point.extend(y);
        Some(PublicKey { kid: j.get("kid")?.as_str()?.to_string(), point })
    }

    pub fn verify(&self, msg: &[u8], sig: &[u8]) -> bool {
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, &self.point).verify(msg, sig).is_ok()
    }
}

async fn generate(pool: &SqlitePool, kek: &Kek, now: i64) -> Result<(), KeyError> {
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).map_err(|_| KeyError::Crypto)?;
    let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).map_err(|_| KeyError::Crypto)?;
    let mut id = [0u8; 8];
    rng.fill(&mut id).map_err(|_| KeyError::Crypto)?;
    let kid = format!("msk_{}", hex::encode(id));
    let sealed = kek.seal(&kid, pkcs8.as_ref())?;
    sqlx::query("INSERT INTO oauth_signing_keys (kid, alg, public_key, private_key_sealed, created_at) VALUES (?, ?, ?, ?, ?)")
        .bind(&kid)
        .bind(ALG)
        .bind(B64URL.encode(pair.public_key().as_ref()))
        .bind(sealed)
        .bind(now)
        .execute(pool)
        .await?;
    Ok(())
}

/// The key that signs now, created on first use.
pub async fn current(pool: &SqlitePool, kek: &Kek, now: i64) -> Result<SigningKey, KeyError> {
    for attempt in 0..2 {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT kid, private_key_sealed FROM oauth_signing_keys WHERE retired_at IS NULL ORDER BY created_at DESC, kid DESC LIMIT 1",
        )
        .fetch_optional(pool)
        .await?;
        match row {
            Some((kid, sealed)) => {
                let pkcs8 = kek.open(&kid, &sealed)?;
                let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &pkcs8, &SystemRandom::new())
                    .map_err(|_| KeyError::Unseal)?;
                return Ok(SigningKey { kid, pair });
            }
            None if attempt == 0 => generate(pool, kek, now).await?,
            None => break,
        }
    }
    Err(KeyError::Crypto)
}

/// Retire the signing key and start a new one. The retired key stays published for
/// `publish_for_secs` (at least the longest token lifetime) so tokens it signed still verify.
pub async fn rotate(pool: &SqlitePool, kek: &Kek, now: i64, publish_for_secs: i64) -> Result<String, KeyError> {
    sqlx::query("UPDATE oauth_signing_keys SET retired_at = ?, publish_until = ? WHERE retired_at IS NULL")
        .bind(now)
        .bind(if crate::accounts::mutant::on("rotation_drops_old") { now } else { now + publish_for_secs })
        .execute(pool)
        .await?;
    generate(pool, kek, now).await?;
    Ok(current(pool, kek, now).await?.kid)
}

/// Every key a verifier should accept now: the signing key and retired keys still in their
/// publication window.
pub async fn published(pool: &SqlitePool, now: i64) -> Result<Vec<PublicKey>, KeyError> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT kid, public_key FROM oauth_signing_keys WHERE retired_at IS NULL OR publish_until > ? ORDER BY created_at DESC, kid",
    )
    .bind(now)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(kid, p)| B64URL.decode(p).ok().map(|point| PublicKey { kid, point }))
        .collect())
}

pub async fn jwks(pool: &SqlitePool, now: i64) -> Result<Value, KeyError> {
    Ok(json!({"keys": published(pool, now).await?.iter().map(PublicKey::jwk).collect::<Vec<_>>()}))
}
