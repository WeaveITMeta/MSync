//! MSync as an OAuth 2.0 authorization server and OpenID Connect provider (contract 12.3).
//!
//! A platform such as Eustress Engine is a registered client. It runs the authorization code
//! flow with PKCE (S256, required for every client) and receives an ID token, a short-lived
//! access token and a rotating refresh token. Standing travels as claims: `groups` (the
//! member's CRA role keys), `titles`, `rank`, `merit`, `name` and `handle`, each only when the
//! scope was granted AND the client's registration allows the claim. `sub` is pairwise per
//! client. Only an active CRA member gets tokens, and a snapshot that suspends or removes a
//! member revokes their token families.
//!
//! Endpoints: `GET /.well-known/openid-configuration`, `GET /oauth/jwks`,
//! `GET|POST /oauth/authorize`, `POST /oauth/token`, `GET|POST /oauth/userinfo`,
//! `POST /oauth/revoke`, `POST /api/v1/oauth/clients`, `POST /api/v1/oauth/keys/rotate`.

pub mod api;
pub mod clients;
pub mod jwt;
pub mod keys;
pub mod server;

#[cfg(test)]
mod tests;

use chrono::NaiveDate;
use serde_json::{Map, Value};
use sqlx::SqlitePool;

use crate::accounts::{self, Provider};
use crate::cra::loader::CraHandle;
use crate::cra::{MemberStatus, Snapshot};

pub use keys::Kek;

/// Scopes a client may register and request.
pub const SCOPES: &[&str] = &["openid", "profile", "groups", "rank", "merit"];
/// Claims a client may be allowed to receive (beyond `sub` and the protocol claims).
pub const CLAIMS: &[&str] = &["name", "handle", "groups", "titles", "rank", "merit"];

/// The claims each scope carries.
pub fn claims_of_scope(scope: &str) -> &'static [&'static str] {
    match scope {
        "profile" => &["name", "handle"],
        "groups" => &["groups", "titles"],
        "rank" => &["rank"],
        "merit" => &["merit"],
        _ => &[],
    }
}

/// An authorization code lives this long.
pub const CODE_TTL_SECS: i64 = 60;
/// Access tokens and ID tokens live this long.
pub const ACCESS_TTL_SECS: i64 = 300;
pub const ID_TTL_SECS: i64 = 300;
/// A refresh token lives this long (it rotates on every use).
pub const REFRESH_TTL_SECS: i64 = 30 * 24 * 3600;
/// A retired signing key stays in the JWKS this long (well past the longest token lifetime).
pub const RETIRED_KEY_PUBLISH_SECS: i64 = 3600;

/// Everything the provider endpoints need.
pub struct OAuthContext {
    pub pool: SqlitePool,
    /// `iss`: `MSYNC_PUBLIC_URL`, no trailing slash.
    pub issuer: String,
    /// `MSYNC_KEY_ENCRYPTION_KEY`. Without it the provider is disabled (503).
    pub kek: Option<Kek>,
    pub cra: CraHandle,
    /// Sign-in providers offered on the "sign in first" page.
    pub sign_in_providers: Vec<Provider>,
    /// Fixed evaluation date (tests). Default: today in UTC.
    pub today: Option<NaiveDate>,
}

impl OAuthContext {
    pub fn from_env(
        pool: SqlitePool,
        public_url: &str,
        cra: CraHandle,
        sign_in_providers: Vec<Provider>,
    ) -> Self {
        let kek = crate::config::env_var("MSYNC_KEY_ENCRYPTION_KEY").filter(|v| !v.trim().is_empty()).and_then(|v| {
            let k = Kek::parse(&v);
            if k.is_none() {
                tracing::error!("MSYNC_KEY_ENCRYPTION_KEY is set but is not 32 bytes (64 hex characters or base64); the OAuth provider is disabled");
            }
            k
        });
        OAuthContext {
            pool,
            issuer: public_url.trim_end_matches('/').to_string(),
            kek,
            cra,
            sign_in_providers,
            today: None,
        }
    }

    pub fn today(&self) -> NaiveDate {
        self.today
            .unwrap_or_else(|| chrono::Utc::now().date_naive())
    }
}

/// The active CRA member an account is, or `None` (no snapshot, not listed, ambiguous, or not
/// active). Only such a member gets tokens.
pub async fn active_member(
    pool: &SqlitePool,
    cra: &CraHandle,
    account_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    let Some(active) = cra.active() else {
        return Ok(None);
    };
    let Some(m) = accounts::member_for_account(pool, &active.snapshot, account_id).await? else {
        return Ok(None);
    };
    let status = active.snapshot.member(&m).map(|x| x.status);
    if status == Some(MemberStatus::Active) || crate::accounts::mutant::on("suspended_gets_tokens")
    {
        Ok(Some(m))
    } else {
        Ok(None)
    }
}

/// The member claims for `scopes`, filtered by the client's allowlist.
pub fn member_claims(
    ctx: &OAuthContext,
    client: &clients::Client,
    member_id: &str,
    scopes: &[String],
) -> Map<String, Value> {
    let mut out = Map::new();
    let Some(active) = ctx.cra.active() else {
        return out;
    };
    let snap = &active.snapshot;
    let Some(m) = snap.member(member_id) else {
        return out;
    };
    let merit = ctx.cra.merit_of(member_id);
    let standing = crate::cra::standing(snap, member_id, merit, ctx.today());
    for scope in scopes {
        for claim in claims_of_scope(scope) {
            if !client.allows_claim(claim) {
                continue;
            }
            let v = match *claim {
                "name" => Value::from(m.name.clone()),
                "handle" => Value::from(m.id.clone()),
                "groups" => Value::from(groups_of(standing.as_ref())),
                "titles" => Value::from(
                    standing
                        .as_ref()
                        .map(|s| s.titles.clone())
                        .unwrap_or_default(),
                ),
                "rank" => Value::from(standing.as_ref().map(|s| s.rank).unwrap_or(0)),
                "merit" => Value::from(merit),
                _ => continue,
            };
            out.insert(claim.to_string(), v);
        }
    }
    out
}

/// A member's groups: the CRA role keys they hold, positions (highest rank first) then the
/// tier. Empty unless active.
pub fn groups_of(st: Option<&crate::cra::standing::Standing>) -> Vec<String> {
    let Some(st) = st else { return Vec::new() };
    if st.status != MemberStatus::Active {
        return Vec::new();
    }
    let mut g: Vec<String> = st.positions.iter().map(|p| p.key.clone()).collect();
    if let Some(t) = &st.tier {
        g.push(t.key.clone());
    }
    g
}

/// Revoke one token family: the grant and every refresh token issued in it.
pub async fn revoke_grant(
    pool: &SqlitePool,
    grant_id: &str,
    reason: &str,
    now: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE oauth_grants SET revoked_at = ?, revoked_reason = ? WHERE id = ? AND revoked_at IS NULL")
        .bind(now)
        .bind(reason)
        .bind(grant_id)
        .execute(pool)
        .await?;
    sqlx::query(
        "UPDATE oauth_refresh_tokens SET revoked_at = ? WHERE grant_id = ? AND revoked_at IS NULL",
    )
    .bind(now)
    .bind(grant_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Called by the CRA loader when a snapshot is activated: every token family whose account is
/// not an active member of `snap` (suspended, removed, or no longer listed) is revoked.
/// Returns how many families were revoked.
pub async fn revoke_inactive_member_grants(
    pool: &SqlitePool,
    snap: &Snapshot,
) -> Result<u64, sqlx::Error> {
    if crate::accounts::mutant::on("no_revoke_at_activation") {
        return Ok(0);
    }
    let now = chrono::Utc::now().timestamp();
    let grants: Vec<(String, String)> =
        sqlx::query_as("SELECT id, account_id FROM oauth_grants WHERE revoked_at IS NULL")
            .fetch_all(pool)
            .await?;
    let mut revoked = 0;
    for (gid, account) in grants {
        let member = accounts::member_for_account(pool, snap, &account).await?;
        let active =
            member.and_then(|m| snap.member(&m).map(|x| x.status)) == Some(MemberStatus::Active);
        if !active {
            revoke_grant(
                pool,
                &gid,
                "the member is not active in the activated CRA snapshot",
                now,
            )
            .await?;
            revoked += 1;
        }
    }
    Ok(revoked)
}

pub fn configure(cfg: &mut actix_web::web::ServiceConfig) {
    server::configure(cfg);
    api::configure(cfg);
}
