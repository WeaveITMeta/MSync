//! `cra_snapshots`: every attempt to load the CRA at one commit (migration
//! `20261114000000_cra_snapshots.sql`).
//!
//! - At most one `active` row per org (partial unique index).
//! - One final outcome per (org, commit) (partial unique index on `retryable = 0`).
//! - Rejected attempts are kept, never deleted.

use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use super::source::CraRepo;
use super::{ChangeContext, CraFiles};

pub const ACTIVE: &str = "active";
pub const SUPERSEDED: &str = "superseded";
pub const REJECTED: &str = "rejected";

/// One recorded reason a load was refused. `code` is the CRA error code, the
/// authorization violation code, or a loader code (`SourceError`, `InvalidCommitSha`,
/// `ActiveSnapshotUnusable`, `ActiveChanged`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedViolation {
    pub code: String,
    pub item: String,
    pub message: String,
}

impl RecordedViolation {
    pub fn new(code: impl Into<String>, item: impl Into<String>, message: impl Into<String>) -> Self {
        RecordedViolation { code: code.into(), item: item.into(), message: message.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SnapshotRow {
    pub id: i64,
    pub org_id: String,
    pub org_slug: Option<String>,
    pub provider: String,
    pub host: String,
    pub repo: String,
    pub commit_sha: String,
    pub content_hash: Option<String>,
    /// The five files, when they were fetched.
    #[serde(skip)]
    pub files: Option<CraFiles>,
    pub status: String,
    pub retryable: bool,
    pub actor: Option<String>,
    pub author: Option<String>,
    pub via_pull_request: bool,
    pub approvers: Vec<String>,
    pub violations: Vec<RecordedViolation>,
    pub plan_markdown: Option<String>,
    pub loaded_at: String,
    pub superseded_at: Option<String>,
}

/// What to write for one attempt.
#[derive(Debug, Clone)]
pub struct Attempt<'a> {
    pub org_id: &'a str,
    pub org_slug: Option<&'a str>,
    pub repo: &'a CraRepo,
    pub commit_sha: &'a str,
    pub files: Option<&'a CraFiles>,
    pub ctx: Option<&'a ChangeContext>,
    pub violations: &'a [RecordedViolation],
    pub plan_markdown: Option<&'a str>,
    pub retryable: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Recorded {
    Inserted(i64),
    /// A final outcome for this (org, commit) already exists.
    AlreadyFinal(Box<SnapshotRow>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Activation {
    Activated(i64),
    /// The active snapshot is no longer the one the change was planned against.
    ActiveChanged { current: Option<i64> },
    AlreadyFinal(Box<SnapshotRow>),
}

const COLUMNS: &str = "id, org_id, org_slug, provider, host, repo, commit_sha, content_hash, org_toml, roles_toml, \
    members_toml, merit_toml, payouts_toml, status, retryable, actor, author, via_pull_request, approvers, violations, \
    plan_markdown, loaded_at, superseded_at";

fn row_to_snapshot(r: &sqlx::sqlite::SqliteRow) -> Result<SnapshotRow, sqlx::Error> {
    let texts: [Option<String>; 5] = [
        r.try_get("org_toml")?,
        r.try_get("roles_toml")?,
        r.try_get("members_toml")?,
        r.try_get("merit_toml")?,
        r.try_get("payouts_toml")?,
    ];
    let files = match texts {
        [Some(org), Some(roles), Some(members), Some(merit), Some(payouts)] => {
            Some(CraFiles { org, roles, members, merit, payouts })
        }
        _ => None,
    };
    let approvers: String = r.try_get("approvers")?;
    let violations: String = r.try_get("violations")?;
    let retryable: i64 = r.try_get("retryable")?;
    let via: i64 = r.try_get("via_pull_request")?;
    Ok(SnapshotRow {
        id: r.try_get("id")?,
        org_id: r.try_get("org_id")?,
        org_slug: r.try_get("org_slug")?,
        provider: r.try_get("provider")?,
        host: r.try_get("host")?,
        repo: r.try_get("repo")?,
        commit_sha: r.try_get("commit_sha")?,
        content_hash: r.try_get("content_hash")?,
        files,
        status: r.try_get("status")?,
        retryable: retryable != 0,
        actor: r.try_get("actor")?,
        author: r.try_get("author")?,
        via_pull_request: via != 0,
        // A row that does not decode reads as "no approvers" / an explicit marker,
        // never as an approval.
        approvers: serde_json::from_str(&approvers).unwrap_or_default(),
        violations: serde_json::from_str(&violations)
            .unwrap_or_else(|_| vec![RecordedViolation::new("Unreadable", "violations", violations.clone())]),
        plan_markdown: r.try_get("plan_markdown")?,
        loaded_at: r.try_get("loaded_at")?,
        superseded_at: r.try_get("superseded_at")?,
    })
}

/// The active snapshot row for an org.
pub async fn active(pool: &SqlitePool, org_id: &str) -> Result<Option<SnapshotRow>, sqlx::Error> {
    let q = format!("SELECT {COLUMNS} FROM cra_snapshots WHERE org_id = ? AND status = 'active'");
    let row = sqlx::query(&q).bind(org_id).fetch_optional(pool).await?;
    row.as_ref().map(row_to_snapshot).transpose()
}

/// The final (non-retryable) outcome recorded for a commit, if any.
pub async fn final_for_commit(pool: &SqlitePool, org_id: &str, sha: &str) -> Result<Option<SnapshotRow>, sqlx::Error> {
    let q = format!("SELECT {COLUMNS} FROM cra_snapshots WHERE org_id = ? AND commit_sha = ? AND retryable = 0");
    let row = sqlx::query(&q).bind(org_id).bind(sha).fetch_optional(pool).await?;
    row.as_ref().map(row_to_snapshot).transpose()
}

pub async fn get(pool: &SqlitePool, id: i64) -> Result<Option<SnapshotRow>, sqlx::Error> {
    let q = format!("SELECT {COLUMNS} FROM cra_snapshots WHERE id = ?");
    let row = sqlx::query(&q).bind(id).fetch_optional(pool).await?;
    row.as_ref().map(row_to_snapshot).transpose()
}

/// Most recent attempts first.
pub async fn recent(pool: &SqlitePool, org_id: &str, limit: i64) -> Result<Vec<SnapshotRow>, sqlx::Error> {
    let q = format!("SELECT {COLUMNS} FROM cra_snapshots WHERE org_id = ? ORDER BY id DESC LIMIT ?");
    let rows = sqlx::query(&q).bind(org_id).bind(limit.clamp(1, 200)).fetch_all(pool).await?;
    rows.iter().map(row_to_snapshot).collect()
}

/// Number of active rows for an org (0 or 1; the index enforces it).
pub async fn active_count(pool: &SqlitePool, org_id: &str) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM cra_snapshots WHERE org_id = ? AND status = 'active'")
        .bind(org_id)
        .fetch_one(pool)
        .await
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.message().contains("UNIQUE"))
}

async fn insert<'c, E>(exec: E, a: &Attempt<'_>, status: &str) -> Result<i64, sqlx::Error>
where
    E: sqlx::Executor<'c, Database = sqlx::Sqlite>,
{
    let f = a.files;
    let approvers: Vec<String> = a
        .ctx
        .filter(|c| c.via_pull_request)
        .map(|c| c.approvers.iter().map(|x| x.to_string()).collect())
        .unwrap_or_default();
    let res = sqlx::query(
        "INSERT INTO cra_snapshots (org_id, org_slug, provider, host, repo, commit_sha, content_hash,
         org_toml, roles_toml, members_toml, merit_toml, payouts_toml, status, retryable, actor, author,
         via_pull_request, approvers, violations, plan_markdown)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(a.org_id)
    .bind(a.org_slug)
    .bind(a.repo.provider.as_str())
    .bind(&a.repo.host)
    .bind(&a.repo.path)
    .bind(a.commit_sha)
    .bind(f.map(|f| f.content_hash()))
    .bind(f.map(|f| f.org.as_str()))
    .bind(f.map(|f| f.roles.as_str()))
    .bind(f.map(|f| f.members.as_str()))
    .bind(f.map(|f| f.merit.as_str()))
    .bind(f.map(|f| f.payouts.as_str()))
    .bind(status)
    .bind(i64::from(a.retryable && status == REJECTED))
    .bind(a.ctx.map(|c| c.actor.to_string()))
    .bind(a.ctx.and_then(|c| c.author.as_ref()).map(|x| x.to_string()))
    .bind(i64::from(a.ctx.map_or(false, |c| c.via_pull_request)))
    .bind(serde_json::to_string(&approvers).unwrap_or_else(|_| "[]".into()))
    .bind(serde_json::to_string(a.violations).unwrap_or_else(|_| "[]".into()))
    .bind(a.plan_markdown)
    .execute(exec)
    .await?;
    Ok(res.last_insert_rowid())
}

/// Record a rejected attempt. The active snapshot is not touched.
pub async fn record_rejected(pool: &SqlitePool, a: &Attempt<'_>) -> Result<Recorded, sqlx::Error> {
    match insert(pool, a, REJECTED).await {
        Ok(id) => Ok(Recorded::Inserted(id)),
        Err(e) if is_unique_violation(&e) => match final_for_commit(pool, a.org_id, a.commit_sha).await? {
            Some(row) => Ok(Recorded::AlreadyFinal(Box::new(row))),
            None => Err(e),
        },
        Err(e) => Err(e),
    }
}

/// Make `a` the active snapshot in one transaction: the current active row (which must
/// be `expected_active`, the one the change was planned and authorized against) becomes
/// superseded and the new row becomes active. Anything else changes nothing.
pub async fn activate(pool: &SqlitePool, a: &Attempt<'_>, expected_active: Option<i64>) -> Result<Activation, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let current: Option<i64> = sqlx::query_scalar("SELECT id FROM cra_snapshots WHERE org_id = ? AND status = 'active'")
        .bind(a.org_id)
        .fetch_optional(&mut *tx)
        .await?;
    if current != expected_active {
        return Ok(Activation::ActiveChanged { current });
    }
    if let Some(old) = current {
        let n = sqlx::query(
            "UPDATE cra_snapshots SET status = 'superseded', superseded_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
             WHERE id = ? AND status = 'active'",
        )
        .bind(old)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n != 1 {
            return Ok(Activation::ActiveChanged { current });
        }
    }
    let id = match insert(&mut *tx, a, ACTIVE).await {
        Ok(id) => id,
        Err(e) if is_unique_violation(&e) => {
            drop(tx); // roll back: the old active row stays active
            return match final_for_commit(pool, a.org_id, a.commit_sha).await? {
                Some(row) => Ok(Activation::AlreadyFinal(Box::new(row))),
                None => {
                    let current = sqlx::query_scalar("SELECT id FROM cra_snapshots WHERE org_id = ? AND status = 'active'")
                        .bind(a.org_id)
                        .fetch_optional(pool)
                        .await?;
                    Ok(Activation::ActiveChanged { current })
                }
            };
        }
        Err(e) => return Err(e),
    };
    tx.commit().await?;
    Ok(Activation::Activated(id))
}
