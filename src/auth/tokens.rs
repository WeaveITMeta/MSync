//! API tokens: `msync_<id>_<secret>`, stored as SHA-256 hex, bound to a member, scoped.

use super::directory::Directory;
use super::scopes;
use actix_web::HttpRequest;
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use subtle::ConstantTimeEq;

/// Org id used until multi-org lands (decision D2: an org id on every new table).
pub const DEFAULT_ORG: &str = "default";
/// The principal the legacy `MSYNC_ADMIN_TOKEN` acts as.
pub const LEGACY_ADMIN_MEMBER: &str = "admin-token";
/// Prefix reserved for engine-internal principals (the scheduler).
pub const SYSTEM_MEMBER_PREFIX: &str = "system:";

const TOKEN_PREFIX: &str = "msync_";
const ID_BYTES: usize = 8; // 16 hex chars
const SECRET_BYTES: usize = 32; // 64 hex chars
const DB_TIME_FMT: &str = "%Y-%m-%d %H:%M:%S";

/// Who is calling. Every write uses `member_id` as the actor; no request body names its own actor.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Principal {
    pub token_id: String,
    pub org_id: String,
    pub member_id: String,
    pub scopes: Vec<String>,
    /// Ledger `github_username` values this member is paid under (lowercased).
    pub payee_keys: Vec<String>,
}

impl Principal {
    pub fn has(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }

    pub fn require(&self, scope: &str) -> Result<(), AuthError> {
        if self.has(scope) {
            Ok(())
        } else {
            Err(AuthError::MissingScope(scope.to_string()))
        }
    }

    /// The legacy `MSYNC_ADMIN_TOKEN` as a principal (`admin-token`). Never a payee.
    pub fn legacy_admin() -> Self {
        Self {
            token_id: "legacy-admin-token".into(),
            org_id: DEFAULT_ORG.into(),
            member_id: LEGACY_ADMIN_MEMBER.into(),
            scopes: scopes::ALL.iter().map(|s| s.to_string()).collect(),
            payee_keys: Vec::new(),
        }
    }

    /// An engine-internal principal such as the payout scheduler (`system:<name>`).
    pub fn system(name: &str, scopes: &[&str]) -> Self {
        Self {
            token_id: format!("{SYSTEM_MEMBER_PREFIX}{name}"),
            org_id: DEFAULT_ORG.into(),
            member_id: format!("{SYSTEM_MEMBER_PREFIX}{name}"),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            payee_keys: Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("missing credentials")]
    Missing,
    #[error("malformed token")]
    Malformed,
    #[error("unknown token")]
    UnknownToken,
    #[error("bad token secret")]
    BadSecret,
    #[error("token revoked")]
    Revoked,
    #[error("token expired")]
    Expired,
    #[error("missing scope {0}")]
    MissingScope(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl AuthError {
    /// True for every failure that must produce the uniform 401 (everything but a missing scope).
    pub fn is_unauthorized(&self) -> bool {
        !matches!(self, AuthError::MissingScope(_))
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct TokenRow {
    id: String,
    org_id: String,
    token_hash: String,
    scopes: String,
    member_id: String,
    payee_keys: String,
    expires_at: Option<String>,
    revoked_at: Option<String>,
}

/// SHA-256 hex of the full token string.
pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Split `msync_<id>_<secret>` into (id, secret); None if the shape is wrong.
pub fn parse_token(token: &str) -> Option<(&str, &str)> {
    let rest = token.strip_prefix(TOKEN_PREFIX)?;
    let (id, secret) = rest.split_once('_')?;
    if is_lower_hex(id, ID_BYTES * 2) && is_lower_hex(secret, SECRET_BYTES * 2) {
        Some((id, secret))
    } else {
        None
    }
}

fn random_hex(n: usize) -> Result<String, getrandom::Error> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf)?;
    Ok(hex::encode(buf))
}

/// The bearer value of the Authorization header, trimmed.
pub fn bearer(req: &HttpRequest) -> Option<&str> {
    req.headers()
        .get("Authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

fn parse_json_list(s: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(s).unwrap_or_default()
}

pub fn format_db_time(t: DateTime<Utc>) -> String {
    t.format(DB_TIME_FMT).to_string()
}

pub fn parse_db_time(s: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(s, DB_TIME_FMT)
        .ok()
        .map(|n| DateTime::<Utc>::from_naive_utc_and_offset(n, Utc))
}

/// Authenticate the request's bearer token against `api_tokens`.
pub async fn authenticate(req: &HttpRequest, pool: &SqlitePool) -> Result<Principal, AuthError> {
    let token = bearer(req).ok_or(AuthError::Missing)?;
    authenticate_token(pool, token, Utc::now()).await
}

/// Authenticate a raw token string at time `now`.
pub async fn authenticate_token(
    pool: &SqlitePool,
    token: &str,
    now: DateTime<Utc>,
) -> Result<Principal, AuthError> {
    let Some((id, _secret)) = parse_token(token) else {
        return Err(AuthError::Malformed);
    };
    let row: Option<TokenRow> = sqlx::query_as(
        "SELECT id, org_id, token_hash, scopes, member_id, payee_keys, expires_at, revoked_at FROM api_tokens WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    let presented = hash_token(token);
    let Some(row) = row else {
        // Same work as a known id so timing does not reveal which ids exist.
        let _ = presented
            .as_bytes()
            .ct_eq(hash_token("msync_unknown").as_bytes());
        return Err(AuthError::UnknownToken);
    };
    if !bool::from(presented.as_bytes().ct_eq(row.token_hash.as_bytes())) {
        return Err(AuthError::BadSecret);
    }
    if row.revoked_at.is_some() {
        return Err(AuthError::Revoked);
    }
    if let Some(exp) = row.expires_at.as_deref() {
        // An unparseable expiry is treated as expired (fail closed).
        match parse_db_time(exp) {
            Some(t) if t > now => {}
            _ => return Err(AuthError::Expired),
        }
    }

    let _ = sqlx::query("UPDATE api_tokens SET last_used_at = ? WHERE id = ?")
        .bind(format_db_time(now))
        .bind(&row.id)
        .execute(pool)
        .await;

    Ok(Principal {
        token_id: row.id,
        org_id: row.org_id,
        member_id: row.member_id,
        scopes: parse_json_list(&row.scopes),
        payee_keys: parse_json_list(&row.payee_keys),
    })
}

/// Who is creating a token.
pub enum TokenCreator<'a> {
    /// The legacy `MSYNC_ADMIN_TOKEN` (bootstrap): may create any token.
    Bootstrap,
    /// A token holder with `tokens.manage`.
    Principal(&'a Principal),
}

#[derive(Debug, Clone)]
pub struct NewToken {
    pub label: String,
    pub member_id: String,
    pub scopes: Vec<String>,
    pub payee_keys: Vec<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// The created token. `token` is shown once and never stored.
#[derive(Debug, Clone, Serialize)]
pub struct CreatedToken {
    pub id: String,
    pub token: String,
    pub label: String,
    pub member_id: String,
    pub scopes: Vec<String>,
    pub payee_keys: Vec<String>,
    pub expires_at: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("{0}")]
    Auth(#[from] AuthError),
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("not found")]
    NotFound,
    #[error("random source failed")]
    Rng,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

fn valid_member_id(m: &str) -> bool {
    !m.is_empty()
        && m.len() <= 64
        && m.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

/// May `creator` act on tokens of `target_member`? Self always; another member only with
/// strictly higher rank (unranked creators can only manage their own tokens).
fn may_manage_member(creator: &Principal, target_member: &str, directory: &dyn Directory) -> bool {
    if creator.member_id == target_member {
        return true;
    }
    match directory.rank_of(&creator.member_id) {
        Some(c) => c > directory.rank_of(target_member).unwrap_or(-1),
        None => false,
    }
}

/// Create a token. Bootstrap may create anything. A principal needs `tokens.manage`, may grant
/// only scopes it holds itself, and may mint for another member only when it strictly outranks them.
pub async fn create_token(
    pool: &SqlitePool,
    directory: &dyn Directory,
    creator: TokenCreator<'_>,
    req: NewToken,
    now: DateTime<Utc>,
) -> Result<CreatedToken, TokenError> {
    let label = req.label.trim().to_string();
    if label.is_empty() || label.len() > 100 {
        return Err(TokenError::Invalid(
            "label must be 1..=100 characters".into(),
        ));
    }
    let member_id = req.member_id.trim().to_string();
    if !valid_member_id(&member_id) {
        return Err(TokenError::Invalid(
            "member_id must be 1..=64 of [A-Za-z0-9._:-]".into(),
        ));
    }
    if member_id == LEGACY_ADMIN_MEMBER || member_id.starts_with(SYSTEM_MEMBER_PREFIX) {
        return Err(TokenError::Invalid(format!(
            "member_id '{member_id}' is reserved"
        )));
    }
    let mut token_scopes: Vec<String> = Vec::new();
    for s in &req.scopes {
        if !scopes::ALL.contains(&s.as_str()) {
            return Err(TokenError::Invalid(format!("unknown scope '{s}'")));
        }
        if !token_scopes.contains(s) {
            token_scopes.push(s.clone());
        }
    }
    if token_scopes.is_empty() {
        return Err(TokenError::Invalid("at least one scope is required".into()));
    }
    let mut payee_keys: Vec<String> = Vec::new();
    for k in &req.payee_keys {
        let k = k.trim().to_lowercase();
        if k.is_empty() || k.len() > 100 || k.chars().any(char::is_whitespace) {
            return Err(TokenError::Invalid(
                "payee_keys must be non-empty, <=100 chars, no whitespace".into(),
            ));
        }
        if !payee_keys.contains(&k) {
            payee_keys.push(k);
        }
    }
    if let Some(exp) = req.expires_at {
        if exp <= now {
            return Err(TokenError::Invalid(
                "expires_at must be in the future".into(),
            ));
        }
    }

    let created_by = match creator {
        TokenCreator::Bootstrap => LEGACY_ADMIN_MEMBER.to_string(),
        TokenCreator::Principal(p) => {
            p.require(scopes::TOKENS_MANAGE)?;
            if let Some(s) = token_scopes.iter().find(|s| !p.has(s)) {
                return Err(TokenError::Forbidden(format!(
                    "cannot grant scope '{s}' you do not hold"
                )));
            }
            if !may_manage_member(p, &member_id, directory) {
                return Err(TokenError::Forbidden(
                    "cannot create a token for a member you do not outrank".into(),
                ));
            }
            p.member_id.clone()
        }
    };

    let id = random_hex(ID_BYTES).map_err(|_| TokenError::Rng)?;
    let secret = random_hex(SECRET_BYTES).map_err(|_| TokenError::Rng)?;
    let token = format!("{TOKEN_PREFIX}{id}_{secret}");
    let expires_at = req.expires_at.map(format_db_time);

    // The token row and its audit entry commit together: no audit entry, no token.
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO api_tokens (id, org_id, label, token_hash, scopes, member_id, payee_keys, created_by, created_at, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(DEFAULT_ORG)
    .bind(&label)
    .bind(hash_token(&token))
    .bind(serde_json::to_string(&token_scopes).unwrap_or_else(|_| "[]".into()))
    .bind(&member_id)
    .bind(serde_json::to_string(&payee_keys).unwrap_or_else(|_| "[]".into()))
    .bind(&created_by)
    .bind(format_db_time(now))
    .bind(&expires_at)
    .execute(&mut *tx)
    .await?;
    let (kind, actor) = audit_actor(&created_by);
    crate::audit::append_in(
        &mut tx,
        &crate::audit::AuditEntry::new(
            kind,
            actor,
            "token.created",
            format!("token:{id}"),
            serde_json::json!({
                "token_id": id,
                "label": label,
                "member_id": member_id,
                "scopes": token_scopes,
                "payee_keys": payee_keys,
                "expires_at": expires_at,
            }),
        ),
    )
    .await?;
    tx.commit().await?;

    Ok(CreatedToken {
        id,
        token,
        label,
        member_id,
        scopes: token_scopes,
        payee_keys,
        expires_at,
    })
}

/// Revoke a token. Same authority rule as creation. Idempotent.
pub async fn revoke_token(
    pool: &SqlitePool,
    directory: &dyn Directory,
    creator: TokenCreator<'_>,
    token_id: &str,
    now: DateTime<Utc>,
) -> Result<(), TokenError> {
    let member: Option<String> =
        sqlx::query_scalar("SELECT member_id FROM api_tokens WHERE id = ?")
            .bind(token_id)
            .fetch_optional(pool)
            .await?;
    let Some(member) = member else {
        return Err(TokenError::NotFound);
    };
    let revoked_by = match creator {
        TokenCreator::Bootstrap => LEGACY_ADMIN_MEMBER.to_string(),
        TokenCreator::Principal(p) => {
            p.require(scopes::TOKENS_MANAGE)?;
            if !may_manage_member(p, &member, directory) {
                return Err(TokenError::Forbidden(
                    "cannot revoke a token of a member you do not outrank".into(),
                ));
            }
            p.member_id.clone()
        }
    };
    // Idempotent: only the first revocation changes the row, and only it is audited, in the
    // same transaction.
    let mut tx = pool.begin().await?;
    let res =
        sqlx::query("UPDATE api_tokens SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL")
            .bind(format_db_time(now))
            .bind(token_id)
            .execute(&mut *tx)
            .await?;
    if res.rows_affected() > 0 {
        let (kind, actor) = audit_actor(&revoked_by);
        crate::audit::append_in(
            &mut tx,
            &crate::audit::AuditEntry::new(
                kind,
                actor,
                "token.revoked",
                format!("token:{token_id}"),
                serde_json::json!({ "token_id": token_id, "member_id": member }),
            ),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The audit actor for a token write made by `member` (`admin-token` for the bootstrap token).
fn audit_actor(member: &str) -> (crate::audit::ActorKind, String) {
    if member == LEGACY_ADMIN_MEMBER {
        (crate::audit::ActorKind::AdminToken, member.to_string())
    } else {
        (crate::audit::ActorKind::Member, member.to_string())
    }
}

/// Every payee key ever declared for a member, across ALL of its tokens (revoked and expired
/// included), lowercased. A second token without payee keys cannot hide that a member is a payee.
pub async fn member_payee_keys(
    pool: &SqlitePool,
    member_id: &str,
) -> Result<Vec<String>, sqlx::Error> {
    let rows: Vec<String> =
        sqlx::query_scalar("SELECT payee_keys FROM api_tokens WHERE member_id = ?")
            .bind(member_id)
            .fetch_all(pool)
            .await?;
    let mut keys: Vec<String> = Vec::new();
    for r in rows {
        for k in parse_json_list(&r) {
            let k = k.to_lowercase();
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
    }
    Ok(keys)
}

/// Is `member_id` the payee of a row paid to `payee_key`? Checks the member's declared payee
/// keys (all tokens) and the directory. Case-insensitive (GitHub logins are).
pub async fn is_payee(
    pool: &SqlitePool,
    directory: &dyn Directory,
    member_id: &str,
    payee_key: &str,
) -> Result<bool, sqlx::Error> {
    let key = payee_key.to_lowercase();
    if let Some(m) = directory.member_for_payee(&key) {
        if m == member_id {
            return Ok(true);
        }
    }
    Ok(member_payee_keys(pool, member_id).await?.contains(&key))
}
