//! Registered platforms (OAuth clients). Created by an owner through
//! `POST /api/v1/oauth/clients` (scope `oauth.clients`). The secret is shown once and stored as
//! SHA-256; a public client has no secret and must use PKCE (every client must, in fact).

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use sqlx::SqlitePool;
use subtle::ConstantTimeEq;

use super::{CLAIMS, SCOPES};
use crate::accounts::mutant;
use crate::accounts::rules::{random_id, random_token, token_hash};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Client {
    pub client_id: String,
    pub name: String,
    #[serde(skip)]
    pub secret_hash: Option<String>,
    pub redirect_uris: Vec<String>,
    pub allowed_scopes: Vec<String>,
    pub allowed_claims: Vec<String>,
    pub consumer_key: Option<String>,
    #[serde(skip)]
    pub pairwise_salt: String,
    pub public: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewClient {
    pub name: String,
    pub redirect_uris: Vec<String>,
    /// A public client (a browser or native app) gets no secret.
    #[serde(default)]
    pub public: bool,
    pub allowed_scopes: Vec<String>,
    #[serde(default)]
    pub allowed_claims: Vec<String>,
    /// The CRA `[[consumer]]` key this platform corresponds to, if any.
    pub consumer_key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreatedClient {
    pub client: Client,
    /// Shown once. `None` for a public client.
    pub client_secret: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("random source failed")]
    Rng,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// An exact redirect URI a client may register: `https://` with a host, or `http://` on a
/// loopback host for local development. No fragment, no wildcard, no whitespace.
pub fn valid_redirect_uri(u: &str) -> bool {
    if u.is_empty() || u.len() > 2000 || u.contains('#') || u.contains('*') || u.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let Some((scheme, rest)) = u.split_once("://") else { return false };
    let host_port = rest.split(['/', '?']).next().unwrap_or("");
    if host_port.is_empty() || host_port.contains('@') {
        return false;
    }
    let host = if host_port.starts_with('[') {
        host_port.split(']').next().map(|h| format!("{h}]")).unwrap_or_default()
    } else {
        host_port.split(':').next().unwrap_or("").to_string()
    };
    match scheme {
        "https" => !host.is_empty(),
        "http" => matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]"),
        _ => false,
    }
}

impl Client {
    /// Exact string match against a registered URI. Nothing else is a match: not a prefix,
    /// not a different query, not a different case.
    pub fn redirect_allowed(&self, uri: &str) -> bool {
        if mutant::on("redirect_prefix") {
            return self.redirect_uris.iter().any(|r| uri.starts_with(r.as_str()));
        }
        self.redirect_uris.iter().any(|r| r == uri)
    }

    /// Confidential clients must present their secret; a public client must present none.
    pub fn authenticate(&self, presented: Option<&str>) -> bool {
        match (&self.secret_hash, presented) {
            (Some(h), Some(s)) => bool::from(token_hash(s).as_bytes().ct_eq(h.as_bytes())),
            (None, None) => true,
            _ => false,
        }
    }

    /// The subject this client sees for an account: HMAC-SHA256 under the client's own salt,
    /// so two clients cannot correlate one person.
    pub fn pairwise_sub(&self, account_id: &str) -> String {
        let key = if mutant::on("sub_not_pairwise") { "shared" } else { self.pairwise_salt.as_str() };
        let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("hmac accepts any key");
        mac.update(b"msync-sub-v1:");
        mac.update(account_id.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    pub fn allows_claim(&self, claim: &str) -> bool {
        mutant::on("claims_ignore_allowlist") || self.allowed_claims.iter().any(|c| c == claim)
    }
}

fn dedupe(v: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in v.iter().map(|s| s.trim().to_string()) {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// Register a client. `consumer_keys` is the active snapshot's `[[consumer]]` keys (None
/// without an active snapshot: the key is stored as given).
pub async fn register(
    pool: &SqlitePool,
    new: NewClient,
    created_by: &str,
    consumer_keys: Option<&[String]>,
    now: i64,
) -> Result<CreatedClient, ClientError> {
    let name = new.name.trim().to_string();
    if name.is_empty() || name.len() > 100 {
        return Err(ClientError::Invalid("name must be 1..=100 characters".into()));
    }
    let redirect_uris = dedupe(&new.redirect_uris);
    if redirect_uris.is_empty() || redirect_uris.len() > 10 {
        return Err(ClientError::Invalid("register 1..=10 redirect URIs".into()));
    }
    if let Some(bad) = redirect_uris.iter().find(|u| !valid_redirect_uri(u)) {
        return Err(ClientError::Invalid(format!("redirect URI '{bad}' must be an exact https:// URL (http:// only on localhost), with no fragment or wildcard")));
    }
    let allowed_scopes = dedupe(&new.allowed_scopes);
    if let Some(bad) = allowed_scopes.iter().find(|s| !SCOPES.contains(&s.as_str())) {
        return Err(ClientError::Invalid(format!("unknown scope '{bad}'; known: {}", SCOPES.join(" "))));
    }
    if !allowed_scopes.iter().any(|s| s == "openid") {
        return Err(ClientError::Invalid("allowed_scopes must include openid".into()));
    }
    let allowed_claims = dedupe(&new.allowed_claims);
    if let Some(bad) = allowed_claims.iter().find(|c| !CLAIMS.contains(&c.as_str())) {
        return Err(ClientError::Invalid(format!("unknown claim '{bad}'; known: {}", CLAIMS.join(" "))));
    }
    let consumer_key = new.consumer_key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
    if let (Some(k), Some(keys)) = (&consumer_key, consumer_keys) {
        if !keys.contains(k) {
            return Err(ClientError::Invalid(format!("consumer_key '{k}' is not a [[consumer]] in the active CRA")));
        }
    }

    let client_id = random_id("msc_").map_err(|_| ClientError::Rng)?;
    let secret = if new.public { None } else { Some(format!("mss_{}", random_token().map_err(|_| ClientError::Rng)?)) };
    let salt = random_token().map_err(|_| ClientError::Rng)?;
    sqlx::query(
        "INSERT INTO oauth_clients (client_id, name, secret_hash, redirect_uris, allowed_scopes, allowed_claims, consumer_key,
         pairwise_salt, created_by, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&client_id)
    .bind(&name)
    .bind(secret.as_deref().map(token_hash))
    .bind(serde_json::to_string(&redirect_uris).unwrap_or_default())
    .bind(serde_json::to_string(&allowed_scopes).unwrap_or_default())
    .bind(serde_json::to_string(&allowed_claims).unwrap_or_default())
    .bind(&consumer_key)
    .bind(&salt)
    .bind(created_by)
    .bind(now)
    .execute(pool)
    .await?;
    let client = get(pool, &client_id).await?.ok_or_else(|| ClientError::Invalid("client vanished".into()))?;
    Ok(CreatedClient { client, client_secret: secret })
}

fn list(s: &str) -> Vec<String> {
    serde_json::from_str(s).unwrap_or_default()
}

type ClientRow = (String, String, Option<String>, String, String, String, Option<String>, String);

/// An active (unrevoked) client.
pub async fn get(pool: &SqlitePool, client_id: &str) -> Result<Option<Client>, sqlx::Error> {
    let r: Option<ClientRow> = sqlx::query_as(
        "SELECT client_id, name, secret_hash, redirect_uris, allowed_scopes, allowed_claims, consumer_key, pairwise_salt
         FROM oauth_clients WHERE client_id = ? AND revoked_at IS NULL",
    )
    .bind(client_id)
    .fetch_optional(pool)
    .await?;
    Ok(r.map(|(client_id, name, secret_hash, ru, sc, cl, consumer_key, pairwise_salt)| Client {
        public: secret_hash.is_none(),
        client_id,
        name,
        secret_hash,
        redirect_uris: list(&ru),
        allowed_scopes: list(&sc),
        allowed_claims: list(&cl),
        consumer_key,
        pairwise_salt,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_uris_must_be_exact_https_or_loopback() {
        for ok in ["https://eustress.example/auth/callback", "https://a.b/cb?x=1", "http://localhost:3000/cb", "http://127.0.0.1/cb", "http://[::1]:8/cb"] {
            assert!(valid_redirect_uri(ok), "{ok}");
        }
        for bad in [
            "",
            "http://eustress.example/cb",
            "https://*.example/cb",
            "https://a.b/cb#frag",
            "https:///cb",
            "javascript:alert(1)",
            "https://user@evil.example/cb",
            "https://a.b/c b",
            "ftp://a.b/cb",
        ] {
            assert!(!valid_redirect_uri(bad), "{bad}");
        }
    }
}
