use super::config::PaymentConfig;
use super::stripe::{StripeApi, TransferRequest};
use serde::Serialize;
use sqlx::SqlitePool;
use tracing::{info, warn};
use uuid::Uuid;

pub const PENDING: &str = "pending_approval";
pub const APPROVED: &str = "approved";
pub const PAID: &str = "paid";
pub const FAILED: &str = "failed";
pub const REJECTED: &str = "rejected";

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct LedgerEntry {
    pub id: String,
    pub pr_id: i64,
    pub pr_number: i64,
    pub merge_sha: String,
    pub repo: Option<String>,
    pub title: String,
    pub github_username: String,
    pub contributor_id: Option<String>,
    pub branch: String,
    pub base_amount_cents: i64,
    pub merit_points: i64,
    pub amount_cents: i64,
    pub currency: String,
    pub status: String,
    pub stripe_transfer_id: Option<String>,
    pub failure_reason: Option<String>,
    pub decided_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct MergeEvent {
    pub pr_id: i64,
    pub pr_number: i64,
    pub merge_sha: String,
    pub repo: Option<String>,
    pub title: String,
    pub github_username: String,
    pub branch: String,
    pub merit_points: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("ledger entry not found")]
    NotFound,
    #[error("ledger entry is '{0}' and cannot be {1}")]
    InvalidState(String, &'static str),
    #[error("not payable: {0}")]
    NotPayable(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Record a merged PR. Returns Some(entry) when a new row was created, None when the
/// branch is untracked or this (pr_id, merge_sha) was already recorded (duplicate webhook).
/// Never moves money.
pub async fn record_merge(pool: &SqlitePool, cfg: &PaymentConfig, ev: &MergeEvent) -> Result<Option<LedgerEntry>, LedgerError> {
    let Some((base, amount)) = cfg.amount_for(&ev.branch, ev.merit_points) else {
        info!("PR #{} merged into untracked branch '{}'; no ledger entry", ev.pr_number, ev.branch);
        return Ok(None);
    };

    let contributor_id: Option<String> =
        sqlx::query_scalar("SELECT id FROM contributors WHERE github_username = ?")
            .bind(&ev.github_username)
            .fetch_optional(pool)
            .await?;

    let id = Uuid::new_v4().to_string();
    let res = sqlx::query(
        "INSERT OR IGNORE INTO payout_ledger (id, pr_id, pr_number, merge_sha, repo, title, github_username,
         contributor_id, branch, base_amount_cents, merit_points, amount_cents, currency, status)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(ev.pr_id)
    .bind(ev.pr_number)
    .bind(&ev.merge_sha)
    .bind(&ev.repo)
    .bind(&ev.title)
    .bind(&ev.github_username)
    .bind(&contributor_id)
    .bind(&ev.branch)
    .bind(base)
    .bind(ev.merit_points)
    .bind(amount)
    .bind(&cfg.currency)
    .bind(PENDING)
    .execute(pool)
    .await?;

    if res.rows_affected() == 0 {
        info!("Duplicate merge event for PR #{} ({}); ledger unchanged", ev.pr_number, ev.merge_sha);
        return Ok(None);
    }
    info!("Ledger entry {} created: PR #{} by {} on {} = {} {}", id, ev.pr_number, ev.github_username, ev.branch, amount, cfg.currency);
    Ok(get(pool, &id).await?)
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<LedgerEntry>, sqlx::Error> {
    sqlx::query_as::<_, LedgerEntry>("SELECT * FROM payout_ledger WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn list(pool: &SqlitePool, status: Option<&str>) -> Result<Vec<LedgerEntry>, sqlx::Error> {
    match status {
        Some(s) => {
            sqlx::query_as::<_, LedgerEntry>("SELECT * FROM payout_ledger WHERE status = ? ORDER BY created_at DESC")
                .bind(s)
                .fetch_all(pool)
                .await
        }
        None => {
            sqlx::query_as::<_, LedgerEntry>("SELECT * FROM payout_ledger ORDER BY created_at DESC")
                .fetch_all(pool)
                .await
        }
    }
}

pub async fn reject(pool: &SqlitePool, id: &str, by: &str, reason: &str) -> Result<LedgerEntry, LedgerError> {
    let entry = get(pool, id).await?.ok_or(LedgerError::NotFound)?;
    let res = sqlx::query(
        "UPDATE payout_ledger SET status = ?, decided_by = ?, failure_reason = ?, updated_at = datetime('now')
         WHERE id = ? AND status IN (?, ?)",
    )
    .bind(REJECTED)
    .bind(by)
    .bind(reason)
    .bind(id)
    .bind(PENDING)
    .bind(FAILED)
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(LedgerError::InvalidState(entry.status, "rejected"));
    }
    get(pool, id).await?.ok_or(LedgerError::NotFound)
}

/// Approve a pending (or previously failed) entry and pay it via a Stripe Transfer.
/// This is the ONLY place a transfer is created. Uses idempotency key `dsync-ledger-<id>`
/// so a retry can never pay twice on Stripe's side.
pub async fn approve(pool: &SqlitePool, stripe: &dyn StripeApi, id: &str, by: &str) -> Result<LedgerEntry, LedgerError> {
    let entry = get(pool, id).await?.ok_or(LedgerError::NotFound)?;
    if entry.status != PENDING && entry.status != FAILED {
        return Err(LedgerError::InvalidState(entry.status, "approved"));
    }
    if entry.amount_cents <= 0 {
        return Err(LedgerError::NotPayable("amount is zero".into()));
    }

    let account: Option<(Option<String>, i64)> = sqlx::query_as(
        "SELECT stripe_account_id, stripe_payouts_enabled FROM contributors WHERE github_username = ?",
    )
    .bind(&entry.github_username)
    .fetch_optional(pool)
    .await?;
    let destination = match account {
        Some((Some(acct), enabled)) if !acct.is_empty() => {
            if enabled == 0 {
                return Err(LedgerError::NotPayable(format!(
                    "{} has not finished Stripe onboarding", entry.github_username
                )));
            }
            acct
        }
        _ => {
            return Err(LedgerError::NotPayable(format!(
                "{} has no Stripe account; onboard via /api/payouts/onboard", entry.github_username
            )))
        }
    };

    // Claim the row atomically so concurrent approvals can't both transfer.
    let claimed = sqlx::query(
        "UPDATE payout_ledger SET status = ?, decided_by = ?, updated_at = datetime('now') WHERE id = ? AND status = ?",
    )
    .bind(APPROVED)
    .bind(by)
    .bind(id)
    .bind(&entry.status)
    .execute(pool)
    .await?;
    if claimed.rows_affected() == 0 {
        let current = get(pool, id).await?.map(|e| e.status).unwrap_or_default();
        return Err(LedgerError::InvalidState(current, "approved"));
    }

    let req = TransferRequest {
        amount_cents: entry.amount_cents,
        currency: entry.currency.clone(),
        destination,
        idempotency_key: format!("dsync-ledger-{}", entry.id),
        ledger_id: entry.id.clone(),
        pr_number: entry.pr_number,
    };

    match stripe.create_transfer(&req).await {
        Ok(transfer_id) => {
            sqlx::query(
                "UPDATE payout_ledger SET status = ?, stripe_transfer_id = ?, failure_reason = NULL,
                 updated_at = datetime('now') WHERE id = ?",
            )
            .bind(PAID)
            .bind(&transfer_id)
            .bind(id)
            .execute(pool)
            .await?;
            info!("Ledger {} paid via transfer {}", id, transfer_id);
        }
        Err(e) => {
            warn!("Transfer for ledger {} failed: {}", id, e);
            sqlx::query("UPDATE payout_ledger SET status = ?, failure_reason = ?, updated_at = datetime('now') WHERE id = ?")
                .bind(FAILED)
                .bind(e.to_string())
                .bind(id)
                .execute(pool)
                .await?;
        }
    }
    get(pool, id).await?.ok_or(LedgerError::NotFound)
}

pub async fn set_stripe_account(pool: &SqlitePool, github_username: &str, account_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE contributors SET stripe_account_id = ?, updated_at = datetime('now') WHERE github_username = ?")
        .bind(account_id)
        .bind(github_username)
        .execute(pool)
        .await?;
    Ok(())
}

/// Returns number of contributors updated (0 if the account is unknown).
pub async fn update_account_status(pool: &SqlitePool, account_id: &str, payouts_enabled: bool) -> Result<u64, sqlx::Error> {
    let res = sqlx::query("UPDATE contributors SET stripe_payouts_enabled = ?, updated_at = datetime('now') WHERE stripe_account_id = ?")
        .bind(payouts_enabled as i64)
        .bind(account_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}