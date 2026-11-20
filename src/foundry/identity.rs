//! Contributors keyed by `(provider, host, login)` (contract 5 and 5.1).
//!
//! `foundry_identities` maps each GitHub or GitLab login to one contributor. The legacy
//! `contributors.github_username` column stays: a GitHub login on github.com is stored
//! bare (as before); a GitLab-only contributor stores `gitlab:<login>` (or
//! `gitlab:<host>:<login>` off gitlab.com), a value no GitHub login can equal.

use super::{legacy_username, FoundryProvider};
use crate::models::Contributor;
use sqlx::SqlitePool;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("contributor not found")]
    UnknownContributor,
    #[error("identity {0} already belongs to another contributor")]
    Conflict(String),
    #[error("login is empty")]
    EmptyLogin,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

fn norm(host: &str, login: &str) -> (String, String) {
    (
        host.trim().to_ascii_lowercase(),
        login.trim().to_ascii_lowercase(),
    )
}

/// Contributor that owns `(provider, host, login)`, if any.
pub async fn find_contributor(
    pool: &SqlitePool,
    provider: FoundryProvider,
    host: &str,
    login: &str,
) -> Result<Option<Contributor>, sqlx::Error> {
    let (host, login_n) = norm(host, login);
    if let Some(c) = sqlx::query_as::<_, Contributor>(
        "SELECT c.* FROM contributors c JOIN foundry_identities f ON f.contributor_id = c.id
         WHERE f.provider = ? AND f.host = ? AND f.login = ?",
    )
    .bind(provider.as_str())
    .bind(&host)
    .bind(&login_n)
    .fetch_optional(pool)
    .await?
    {
        return Ok(Some(c));
    }
    sqlx::query_as::<_, Contributor>("SELECT * FROM contributors WHERE github_username = ?")
        .bind(legacy_username(provider, &host, login.trim()))
        .fetch_optional(pool)
        .await
}

/// Get or create the contributor for a GitHub or GitLab login, and record the identity.
pub async fn resolve_contributor(
    pool: &SqlitePool,
    provider: FoundryProvider,
    host: &str,
    login: &str,
) -> Result<Contributor, sqlx::Error> {
    let contributor = match find_contributor(pool, provider, host, login).await? {
        Some(c) => c,
        None => {
            let c = Contributor::new(legacy_username(provider, host, login.trim()));
            sqlx::query(
                "INSERT INTO contributors (id, github_username, total_merit_points, current_role,
                 contributions_count, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(c.id)
            .bind(&c.github_username)
            .bind(c.total_merit_points)
            .bind(&c.current_role)
            .bind(c.contributions_count)
            .bind(c.created_at)
            .bind(c.updated_at)
            .execute(pool)
            .await?;
            c
        }
    };
    let (host_n, login_n) = norm(host, login);
    // Copies contributors.id as stored, so the join above matches its storage type.
    sqlx::query(
        "INSERT OR IGNORE INTO foundry_identities (provider, host, login, contributor_id)
         SELECT ?, ?, ?, id FROM contributors WHERE id = ?",
    )
    .bind(provider.as_str())
    .bind(&host_n)
    .bind(&login_n)
    .bind(contributor.id)
    .execute(pool)
    .await?;
    Ok(contributor)
}

/// Attach another GitHub or GitLab identity to an existing contributor, so XP from both adds up.
/// Idempotent for the same contributor. Refuses (never merges) an identity that
/// already belongs to a different contributor, through `foundry_identities` or the
/// legacy `github_username` key.
pub async fn link_foundry_identity(
    pool: &SqlitePool,
    contributor_id: Uuid,
    provider: FoundryProvider,
    host: &str,
    login: &str,
) -> Result<(), LinkError> {
    let (host_n, login_n) = norm(host, login);
    if login_n.is_empty() || host_n.is_empty() {
        return Err(LinkError::EmptyLogin);
    }
    let label = format!("{}:{}:{}", provider, host_n, login_n);
    let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM contributors WHERE id = ?")
        .bind(contributor_id)
        .fetch_one(pool)
        .await?;
    if exists == 0 {
        return Err(LinkError::UnknownContributor);
    }

    let owner_is_target: Option<bool> = sqlx::query_scalar(
        "SELECT contributor_id = ? FROM foundry_identities WHERE provider = ? AND host = ? AND login = ?",
    )
    .bind(contributor_id)
    .bind(provider.as_str())
    .bind(&host_n)
    .bind(&login_n)
    .fetch_optional(pool)
    .await?;
    match owner_is_target {
        Some(true) => return Ok(()),
        Some(false) => return Err(LinkError::Conflict(label)),
        None => {}
    }

    let legacy_owner_is_target: Option<bool> =
        sqlx::query_scalar("SELECT id = ? FROM contributors WHERE github_username = ?")
            .bind(contributor_id)
            .bind(legacy_username(provider, &host_n, login.trim()))
            .fetch_optional(pool)
            .await?;
    if legacy_owner_is_target == Some(false) {
        return Err(LinkError::Conflict(label));
    }

    let res = sqlx::query(
        "INSERT OR IGNORE INTO foundry_identities (provider, host, login, contributor_id)
         SELECT ?, ?, ?, id FROM contributors WHERE id = ?",
    )
    .bind(provider.as_str())
    .bind(&host_n)
    .bind(&login_n)
    .bind(contributor_id)
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        // Lost a race to another link of the same identity.
        return Err(LinkError::Conflict(label));
    }
    Ok(())
}
