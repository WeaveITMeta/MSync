//! Chat accounts bound to contributors (`chat_identities`, contract 12.2).
//!
//! Keyed by `(platform, team_or_guild, user_id)`. One contributor may hold a Discord id and
//! a Slack id; both lead to the same `contributors` row. Rows point at a contributor through
//! `contributors.id` as stored (copied with `INSERT ... SELECT id`), and every query here
//! names the contributor by its unique `github_username` key, so the storage type of
//! `contributors.id` never matters.

use sqlx::SqlitePool;

use super::{ChatActor, ChatPlatform};

/// A chat account's binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// The contributor's `github_username` key (`alice`, or `gitlab:alice` for GitLab).
    pub contributor: String,
    /// Proven (a future OAuth link). Every binding made by `link`, and every binding from
    /// before chat identities existed, is unverified.
    pub verified: bool,
}

/// The contributor a chat account is bound to, if any.
pub async fn binding_for(pool: &SqlitePool, actor: &ChatActor) -> Result<Option<Binding>, sqlx::Error> {
    let row: Option<(String, i64)> = sqlx::query_as(
        "SELECT c.github_username, ci.verified FROM chat_identities ci
         JOIN contributors c ON c.id = ci.contributor_id
         WHERE ci.platform = ? AND ci.team_or_guild = ? AND ci.user_id = ?",
    )
    .bind(actor.platform.as_str())
    .bind(actor.storage_team())
    .bind(&actor.user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(contributor, v)| Binding { contributor, verified: v != 0 }))
}

/// Every `(team_or_guild, user_id)` on `platform` bound to the contributor `contributor_key`.
pub async fn accounts_of(pool: &SqlitePool, platform: ChatPlatform, contributor_key: &str) -> Result<Vec<(String, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT ci.team_or_guild, ci.user_id FROM chat_identities ci
         JOIN contributors c ON c.id = ci.contributor_id
         WHERE ci.platform = ? AND c.github_username = ?
         ORDER BY ci.team_or_guild, ci.user_id",
    )
    .bind(platform.as_str())
    .bind(contributor_key)
    .fetch_all(pool)
    .await
}

/// Bind the actor's chat account to an existing contributor. Returns false (and writes
/// nothing) when the account is already bound, when the contributor already has an account
/// on this platform and team, or when the contributor does not exist: the unique keys make
/// each of those a no-op, so a race can never produce a second binding.
pub async fn bind(
    pool: &SqlitePool,
    actor: &ChatActor,
    contributor_key: &str,
    verified: bool,
    source: &str,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "INSERT OR IGNORE INTO chat_identities (platform, team_or_guild, user_id, contributor_id, verified, source)
         SELECT ?, ?, ?, id, ?, ? FROM contributors WHERE github_username = ?",
    )
    .bind(actor.platform.as_str())
    .bind(actor.storage_team())
    .bind(&actor.user_id)
    .bind(i64::from(verified))
    .bind(source)
    .bind(contributor_key)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() == 1)
}
