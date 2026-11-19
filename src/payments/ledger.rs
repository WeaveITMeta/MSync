use super::config::PaymentConfig;
use super::policy;
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

/// Ledger sources (contract 11.5). Existing rows are `merit`.
pub const SOURCE_MERIT: &str = "merit";
pub const SOURCE_PAY_ORDER: &str = "pay_order";
pub const SOURCE_INVOICE: &str = "invoice";

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
    /// merit | pay_order | invoice
    pub source_kind: String,
    /// The pay order occurrence id or the invoice id (None for merit rows).
    pub source_ref: Option<String>,
    /// payees.id for pay_order and invoice rows.
    pub payee_id: Option<String>,
    /// Not payable before this time (`YYYY-MM-DD HH:MM:SS` UTC): an occurrence date or an
    /// invoice due date. None for merit rows.
    pub pay_after: Option<String>,
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
    /// The stored payout policy could not be loaded (fail closed).
    #[error("payout policy unavailable: {0}")]
    Policy(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Conditions checked inside the atomic status claim (contract section 8: limits are re-checked
/// at execute time, inside the same claim that marks the row). Every field is optional.
#[derive(Debug, Clone, Default)]
pub struct ClaimGuard {
    pub min_cents: Option<i64>,
    pub max_cents: Option<i64>,
    /// (window_start, window_end, cap): amounts of this payee's rows that are approved/paid with
    /// updated_at in [start, end), plus this row, must not exceed cap. Times as `YYYY-MM-DD HH:MM:SS`.
    pub member_period: Option<(String, String, i64)>,
    /// (run_id, cap): amounts of this run's rows that are approved/paid, plus this row, must not exceed cap.
    pub run_cap: Option<(String, i64)>,
    /// Refuse the claim when the payee is in payout_blocks.
    pub check_blocks: bool,
    /// Timestamp written to updated_at by the claim (defaults to datetime('now')).
    pub now: Option<String>,
}

/// Record a merged PR. Returns Some(entry) when a new row was created, None when the
/// branch is untracked or this (pr_id, merge_sha) was already recorded (duplicate webhook).
/// Never moves money.
pub async fn record_merge(pool: &SqlitePool, cfg: &PaymentConfig, ev: &MergeEvent) -> Result<Option<LedgerEntry>, LedgerError> {
    // Rates, cap and currency come from the effective payout policy. With no stored policy that is
    // PayoutPolicy::from_config(cfg), whose amount_for equals cfg.amount_for (tested).
    let effective = policy::load_effective(pool, crate::auth::DEFAULT_ORG, cfg)
        .await
        .map_err(|e| LedgerError::Policy(e.to_string()))?;
    let currency = effective.policy.currency.clone();
    let Some((base, amount)) = effective.policy.amount_for(&ev.branch, ev.merit_points) else {
        info!("PR #{} merged into untracked branch '{}'; no ledger entry", ev.pr_number, ev.branch);
        return Ok(None);
    };

    let contributor_id: Option<String> =
        // contributors.id may be a BLOB (sqlx Uuid encoding) or TEXT; normalise to UUID text
        sqlx::query_scalar("SELECT CASE WHEN typeof(id) = 'blob' THEN lower(substr(hex(id),1,8)||'-'||substr(hex(id),9,4)||'-'||substr(hex(id),13,4)||'-'||substr(hex(id),17,4)||'-'||substr(hex(id),21)) ELSE id END FROM contributors WHERE github_username = ?")
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
    .bind(&currency)
    .bind(PENDING)
    .execute(pool)
    .await?;

    if res.rows_affected() == 0 {
        info!("Duplicate merge event for PR #{} ({}); ledger unchanged", ev.pr_number, ev.merge_sha);
        return Ok(None);
    }
    info!("Ledger entry {} created: PR #{} by {} on {} = {} {}", id, ev.pr_number, ev.github_username, ev.branch, amount, currency);
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

/// Approve a pending (or previously failed) entry and pay it via a Stripe Transfer, with no
/// policy guard (the pre-policy behaviour, used by the legacy route when no policy is stored).
pub async fn approve(pool: &SqlitePool, stripe: &dyn StripeApi, id: &str, by: &str) -> Result<LedgerEntry, LedgerError> {
    claim_and_transfer(pool, stripe, id, by, None).await
}

/// Pay a pending (or previously failed) entry only if `guard` holds inside the atomic claim.
/// Returns NotPayable when the guard refuses the claim.
pub async fn pay_guarded(
    pool: &SqlitePool,
    stripe: &dyn StripeApi,
    id: &str,
    by: &str,
    guard: &ClaimGuard,
) -> Result<LedgerEntry, LedgerError> {
    claim_and_transfer(pool, stripe, id, by, Some(guard)).await
}

/// The ONLY place a transfer is created. Claims the row atomically (status pending/failed ->
/// approved, plus any guard conditions in the same UPDATE) and uses idempotency key
/// `dsync-ledger-<id>` so a retry can never pay twice on Stripe's side.
async fn claim_and_transfer(
    pool: &SqlitePool,
    stripe: &dyn StripeApi,
    id: &str,
    by: &str,
    guard: Option<&ClaimGuard>,
) -> Result<LedgerEntry, LedgerError> {
    let entry = get(pool, id).await?.ok_or(LedgerError::NotFound)?;
    if entry.status != PENDING && entry.status != FAILED {
        return Err(LedgerError::InvalidState(entry.status, "approved"));
    }
    if entry.amount_cents <= 0 {
        return Err(LedgerError::NotPayable("amount is zero".into()));
    }

    // Conditions that hold for every source and every caller, guarded or not (contract 11.5):
    // a row is never paid before its pay_after time, and a payee that is paused or closed is
    // never paid. Both are re-checked inside the claim below.
    let now_s = guard
        .and_then(|g| g.now.clone())
        .unwrap_or_else(|| crate::auth::tokens::format_db_time(chrono::Utc::now()));
    if let Some(after) = &entry.pay_after {
        if after.as_str() > now_s.as_str() {
            return Err(LedgerError::NotPayable(format!("not payable before {after}")));
        }
    }
    let inactive: Option<String> = sqlx::query_scalar(
        "SELECT p.status FROM payees p WHERE p.status <> 'active' AND (p.id = ? OR p.id IN
         (SELECT k.payee_id FROM payee_keys k WHERE k.payee_key = lower(?))) LIMIT 1",
    )
    .bind(&entry.payee_id)
    .bind(&entry.github_username)
    .fetch_optional(pool)
    .await?;
    if let Some(st) = inactive {
        return Err(LedgerError::NotPayable(format!("payee is {st}")));
    }

    let destination = if let Some(payee_id) = &entry.payee_id {
        // Pay order and invoice rows are paid to the payee's own Stripe Connect account.
        let acct: Option<(Option<String>, i64)> =
            sqlx::query_as("SELECT stripe_account_id, stripe_payouts_enabled FROM payees WHERE id = ?")
                .bind(payee_id)
                .fetch_optional(pool)
                .await?;
        match acct {
            Some((Some(a), enabled)) if !a.is_empty() => {
                if enabled == 0 {
                    return Err(LedgerError::NotPayable("payee's Stripe account cannot receive transfers yet".into()));
                }
                a
            }
            Some(_) => return Err(LedgerError::NotPayable("payee has no Stripe account".into())),
            None => return Err(LedgerError::NotPayable("payee not found".into())),
        }
    } else {
        let account: Option<(Option<String>, i64)> = sqlx::query_as(
            "SELECT stripe_account_id, stripe_payouts_enabled FROM contributors WHERE github_username = ?",
        )
        .bind(&entry.github_username)
        .fetch_optional(pool)
        .await?;
        match account {
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
        }
    };

    // Claim the row atomically so concurrent approvals can't both transfer. Guard conditions
    // are part of the same UPDATE, so a limit cannot be raced between check and claim.
    let mut sql = String::from(
        "UPDATE payout_ledger SET status = ?, decided_by = ?, updated_at = COALESCE(?, datetime('now')) WHERE id = ? AND status = ?",
    );
    // Always on (no-ops for merit rows with no payee record: pay_after and payee_id are NULL).
    sql.push_str(" AND (payout_ledger.pay_after IS NULL OR payout_ledger.pay_after <= ?)");
    sql.push_str(
        " AND NOT EXISTS (SELECT 1 FROM payees py WHERE py.status <> 'active' AND (py.id = payout_ledger.payee_id \
         OR py.id IN (SELECT k.payee_id FROM payee_keys k WHERE k.payee_key = lower(payout_ledger.github_username))))",
    );
    sql.push_str(
        " AND (payout_ledger.payee_id IS NULL OR EXISTS (SELECT 1 FROM payees py WHERE py.id = payout_ledger.payee_id \
         AND py.status = 'active' AND py.stripe_payouts_enabled = 1 AND py.stripe_account_id = ?))",
    );
    if let Some(g) = guard {
        if g.min_cents.is_some() {
            sql.push_str(" AND amount_cents >= ?");
        }
        if g.max_cents.is_some() {
            sql.push_str(" AND amount_cents <= ?");
        }
        if g.check_blocks {
            sql.push_str(
                " AND NOT EXISTS (SELECT 1 FROM payout_blocks b WHERE b.payee_key = lower(payout_ledger.github_username) \
                 OR b.payee_key IN (SELECT k.payee_key FROM payee_keys k WHERE k.payee_id = payout_ledger.payee_id))",
            );
        }
        if g.member_period.is_some() {
            sql.push_str(&format!(
                " AND amount_cents + (SELECT COALESCE(SUM(p.amount_cents), 0) FROM payout_ledger p \
                 WHERE lower(p.github_username) = lower(payout_ledger.github_username) AND p.id <> payout_ledger.id \
                 AND p.status IN ('{APPROVED}', '{PAID}') AND p.updated_at >= ? AND p.updated_at < ?) <= ?"
            ));
        }
        if g.run_cap.is_some() {
            sql.push_str(&format!(
                " AND amount_cents + (SELECT COALESCE(SUM(p.amount_cents), 0) FROM payout_ledger p \
                 JOIN payout_run_items i ON i.ledger_id = p.id WHERE i.run_id = ? AND p.id <> payout_ledger.id \
                 AND p.status IN ('{APPROVED}', '{PAID}')) <= ?"
            ));
        }
    }
    let mut q = sqlx::query(&sql)
        .bind(APPROVED)
        .bind(by)
        .bind(guard.and_then(|g| g.now.clone()))
        .bind(id)
        .bind(&entry.status)
        .bind(&now_s)
        .bind(&destination);
    if let Some(g) = guard {
        if let Some(min) = g.min_cents {
            q = q.bind(min);
        }
        if let Some(max) = g.max_cents {
            q = q.bind(max);
        }
        if let Some((start, end, cap)) = &g.member_period {
            q = q.bind(start.clone()).bind(end.clone()).bind(*cap);
        }
        if let Some((run_id, cap)) = &g.run_cap {
            q = q.bind(run_id.clone()).bind(*cap);
        }
    }
    let claimed = q.execute(pool).await?;
    if claimed.rows_affected() == 0 {
        let current = get(pool, id).await?.map(|e| e.status).unwrap_or_default();
        if current == entry.status {
            return Err(LedgerError::NotPayable(if guard.is_some() {
                "a limit, block, payee status or pay_after time refused the claim".into()
            } else {
                "the payee status or pay_after time refused the claim".into()
            }));
        }
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
    // A payee linked by this payee key and without an account of its own takes the same
    // account, not yet enabled (only the Stripe webhook enables it).
    sqlx::query(
        "UPDATE payees SET stripe_account_id = ?, stripe_payouts_enabled = 0, updated_at = datetime('now')
         WHERE stripe_account_id IS NULL AND id IN (SELECT payee_id FROM payee_keys WHERE payee_key = lower(?))",
    )
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
    // Payees paid to the same Connect account follow the same state (contract 11.2).
    sqlx::query("UPDATE payees SET stripe_payouts_enabled = ?, updated_at = datetime('now') WHERE stripe_account_id = ?")
        .bind(payouts_enabled as i64)
        .bind(account_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// A non-merit ledger row (contract 11.5). The only way pay orders and invoices write the
/// ledger. Never moves money.
#[derive(Debug, Clone)]
pub struct SourceRow {
    /// SOURCE_PAY_ORDER or SOURCE_INVOICE.
    pub source_kind: &'static str,
    /// Occurrence id or invoice id.
    pub source_ref: String,
    pub payee_id: String,
    /// The payee key written to github_username: the payee's first key, or `payee:<id>`.
    pub payee_key: String,
    pub contributor_id: Option<String>,
    pub title: String,
    pub amount_cents: i64,
    pub currency: String,
    pub pay_after: Option<String>,
    pub created_at: String,
}

/// The legacy NOT NULL columns of a non-merit row, chosen so they can never collide with a real
/// merge under UNIQUE (pr_id, merge_sha): pr_id is negative (real ids are positive) and the
/// merge_sha contains ':' (a real merge sha is hexadecimal). Returns (pr_id, merge_sha).
pub fn legacy_columns(source_kind: &str, source_ref: &str, payee_id: &str) -> (i64, String) {
    match source_kind {
        SOURCE_INVOICE => (-2, format!("invoice:{source_ref}")),
        _ => (-1, format!("pay_order:{source_ref}:{payee_id}")),
    }
}

/// Insert a non-merit row inside the caller's transaction. Returns None when the row for this
/// (source_kind, source_ref, payee_id) already exists (it is never written twice).
pub async fn insert_source_row(conn: &mut sqlx::SqliteConnection, row: &SourceRow) -> Result<Option<String>, sqlx::Error> {
    let id = Uuid::new_v4().to_string();
    let (pr_id, merge_sha) = legacy_columns(row.source_kind, &row.source_ref, &row.payee_id);
    let res = sqlx::query(
        "INSERT INTO payout_ledger (id, pr_id, pr_number, merge_sha, repo, title, github_username,
         contributor_id, branch, base_amount_cents, merit_points, amount_cents, currency, status,
         created_at, updated_at, source_kind, source_ref, payee_id, pay_after)
         VALUES (?, ?, 0, ?, NULL, ?, ?, ?, ?, ?, 0, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT DO NOTHING",
    )
    .bind(&id)
    .bind(pr_id)
    .bind(&merge_sha)
    .bind(&row.title)
    .bind(&row.payee_key)
    .bind(&row.contributor_id)
    .bind(row.source_kind)
    .bind(row.amount_cents)
    .bind(row.amount_cents)
    .bind(&row.currency)
    .bind(PENDING)
    .bind(&row.created_at)
    .bind(&row.created_at)
    .bind(row.source_kind)
    .bind(&row.source_ref)
    .bind(&row.payee_id)
    .bind(&row.pay_after)
    .execute(&mut *conn)
    .await?;
    Ok(if res.rows_affected() == 0 { None } else { Some(id) })
}