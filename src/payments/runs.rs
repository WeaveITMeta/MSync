//! Payout runs (contract section 8): a set of ledger rows evaluated together.
//!
//! Plain async functions over a pool and a `Principal`, so the REST layer and a later MCP layer
//! share them. Money only moves through `ledger::pay_guarded` (the atomic claim + idempotency key).

use super::approvals::{self, claim_guard, evaluate, load_facts, member_period_usage, payee_key, principal_is_payee, Blocker, RowApproval, Usage};
use super::config::PaymentConfig;
use super::error::PayoutError;
use super::ledger::{self, LedgerEntry, LedgerError, FAILED, PAID, PENDING};
use super::policy::{self, PayoutPolicy};
use super::schedule::period_window;
use super::stripe::StripeApi;
use crate::auth::{scopes, tokens::format_db_time, Directory, Principal};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::SqlitePool;
use uuid::Uuid;

pub const RUN_PREPARED: &str = "prepared";
pub const RUN_COMPLETED: &str = "completed";
pub const RUN_CANCELLED: &str = "cancelled";

#[derive(Debug, Clone, Serialize)]
pub struct PlanItem {
    pub ledger_id: String,
    pub payee_key: String,
    pub pr_number: i64,
    pub amount_cents: i64,
    /// "include" or "skip"
    pub decision: String,
    /// Why the row is skipped, or what it still waits for.
    pub reason: Option<String>,
    pub approvals: i64,
    pub approvals_required: i64,
    pub payable_now: bool,
    pub blockers: Vec<Blocker>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunPlan {
    pub dry_run: bool,
    pub policy_version: Option<i64>,
    pub enabled: bool,
    pub items: Vec<PlanItem>,
    pub included_total_cents: i64,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct RunRow {
    pub id: String,
    pub org_id: String,
    pub period_key: Option<String>,
    pub trigger_kind: String,
    pub status: String,
    pub policy_version: Option<i64>,
    pub created_by: String,
    pub created_at: String,
    pub cancelled_by: Option<String>,
    pub cancelled_at: Option<String>,
    pub last_executed_by: Option<String>,
    pub last_executed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct RunItemRow {
    pub ledger_id: String,
    pub payee_key: String,
    pub amount_cents: i64,
    pub decision: String,
    pub reason: Option<String>,
    pub outcome: Option<String>,
    pub outcome_reason: Option<String>,
    pub executed_at: Option<String>,
    pub ledger_status: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunView {
    pub run: RunRow,
    pub items: Vec<RunItemRow>,
}

/// Result of `create_run`: a plan only (dry run) or a persisted run.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum CreateRunResult {
    DryRun(RunPlan),
    Created { run: RunView, plan: RunPlan },
}

async fn open_rows(pool: &SqlitePool) -> Result<Vec<LedgerEntry>, sqlx::Error> {
    sqlx::query_as::<_, LedgerEntry>(
        "SELECT * FROM payout_ledger WHERE status IN (?, ?) ORDER BY created_at ASC, id ASC",
    )
    .bind(PENDING)
    .bind(FAILED)
    .fetch_all(pool)
    .await
}

/// Evaluate every open ledger row against `policy` at `now`, oldest first. Included rows (those
/// blocked by nothing but missing approvals) consume the run and per-member budgets in order.
pub async fn build_plan(
    pool: &SqlitePool,
    directory: &dyn Directory,
    policy: &PayoutPolicy,
    policy_version: Option<i64>,
    now: DateTime<Utc>,
    dry_run: bool,
) -> Result<RunPlan, PayoutError> {
    let (start, end) = period_window(&policy.schedule, now);
    let (start, end) = (format_db_time(start), format_db_time(end));
    let mut usage = Usage::default();
    let mut items = Vec::new();
    let mut total = 0;
    for entry in open_rows(pool).await? {
        let key = payee_key(&entry);
        if !usage.member_period.contains_key(&key) {
            let used = member_period_usage(pool, &key, &start, &end).await?;
            usage.member_period.insert(key.clone(), used);
        }
        let facts = load_facts(pool, directory, entry).await?;
        let blockers = evaluate(policy, &facts, now, &usage);
        let include = blockers.iter().all(Blocker::is_approvals);
        let e = &facts.entry;
        if include {
            usage.add(&key, e.amount_cents);
            total += e.amount_cents;
        }
        let reason = if blockers.is_empty() {
            None
        } else {
            Some(blockers.iter().map(Blocker::message).collect::<Vec<_>>().join("; "))
        };
        items.push(PlanItem {
            ledger_id: e.id.clone(),
            payee_key: key,
            pr_number: e.pr_number,
            amount_cents: e.amount_cents,
            decision: if include { "include" } else { "skip" }.into(),
            reason,
            approvals: approvals::counted_approvals(policy, &facts.approvers),
            approvals_required: policy.approval.approvals_required,
            payable_now: blockers.is_empty(),
            blockers,
        });
    }
    Ok(RunPlan {
        dry_run,
        policy_version,
        enabled: policy.enabled,
        items,
        included_total_cents: total,
    })
}

async fn persist_run(
    pool: &SqlitePool,
    plan: &RunPlan,
    principal: &Principal,
    period_key: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Option<String>, PayoutError> {
    let id = Uuid::new_v4().to_string();
    let mut tx = pool.begin().await?;
    let res = sqlx::query(
        "INSERT OR IGNORE INTO payout_runs (id, org_id, period_key, trigger_kind, status, policy_version, created_by, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&principal.org_id)
    .bind(period_key)
    .bind(if period_key.is_some() { "scheduled" } else { "manual" })
    .bind(RUN_PREPARED)
    .bind(plan.policy_version)
    .bind(&principal.member_id)
    .bind(format_db_time(now))
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() == 0 {
        // The period already has its run.
        return Ok(None);
    }
    for it in &plan.items {
        sqlx::query(
            "INSERT INTO payout_run_items (run_id, ledger_id, org_id, payee_key, amount_cents, decision, reason)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&it.ledger_id)
        .bind(&principal.org_id)
        .bind(&it.payee_key)
        .bind(it.amount_cents)
        .bind(&it.decision)
        .bind(&it.reason)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(Some(id))
}

/// POST /payouts/runs (`payouts.prepare`). `dry_run = true` returns the plan and persists nothing.
/// `dry_run = false` persists a prepared run; refused while the policy is disabled.
pub async fn create_run(
    pool: &SqlitePool,
    cfg: &PaymentConfig,
    directory: &dyn Directory,
    principal: &Principal,
    dry_run: bool,
    now: DateTime<Utc>,
) -> Result<CreateRunResult, PayoutError> {
    principal.require(scopes::PAYOUTS_PREPARE)?;
    let eff = policy::load_effective(pool, &principal.org_id, cfg).await?;
    let plan = build_plan(pool, directory, &eff.policy, eff.version, now, dry_run).await?;
    if dry_run {
        return Ok(CreateRunResult::DryRun(plan));
    }
    if !eff.policy.enabled {
        return Err(PayoutError::PolicyDisabled);
    }
    let id = persist_run(pool, &plan, principal, None, now)
        .await?
        .ok_or_else(|| PayoutError::InvalidState("run id collision".into()))?;
    let run = load_run(pool, &id).await?.ok_or(PayoutError::NotFound("run"))?;
    Ok(CreateRunResult::Created { run, plan })
}

/// Create the scheduled run for `period_key`. Returns None when the period already has one.
pub async fn create_scheduled_run(
    pool: &SqlitePool,
    cfg: &PaymentConfig,
    directory: &dyn Directory,
    principal: &Principal,
    period_key: &str,
    now: DateTime<Utc>,
) -> Result<Option<String>, PayoutError> {
    principal.require(scopes::PAYOUTS_PREPARE)?;
    let eff = policy::load_effective(pool, &principal.org_id, cfg).await?;
    if !eff.policy.enabled {
        return Ok(None);
    }
    let plan = build_plan(pool, directory, &eff.policy, eff.version, now, false).await?;
    persist_run(pool, &plan, principal, Some(period_key), now).await
}

async fn load_run(pool: &SqlitePool, id: &str) -> Result<Option<RunView>, sqlx::Error> {
    let Some(run) = sqlx::query_as::<_, RunRow>(
        "SELECT id, org_id, period_key, trigger_kind, status, policy_version, created_by, created_at, cancelled_by,
         cancelled_at, last_executed_by, last_executed_at FROM payout_runs WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };
    let items = sqlx::query_as::<_, RunItemRow>(
        "SELECT i.ledger_id, i.payee_key, i.amount_cents, i.decision, i.reason, i.outcome, i.outcome_reason, i.executed_at,
         l.status AS ledger_status
         FROM payout_run_items i LEFT JOIN payout_ledger l ON l.id = i.ledger_id
         WHERE i.run_id = ? ORDER BY l.created_at ASC, i.ledger_id ASC",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    Ok(Some(RunView { run, items }))
}

/// GET /payouts/runs/{id} (`payouts.read`).
pub async fn get_run(pool: &SqlitePool, principal: &Principal, id: &str) -> Result<RunView, PayoutError> {
    principal.require(scopes::PAYOUTS_READ)?;
    let view = load_run(pool, id).await?.ok_or(PayoutError::NotFound("run"))?;
    if view.run.org_id != principal.org_id {
        return Err(PayoutError::NotFound("run"));
    }
    Ok(view)
}

#[derive(Debug, Clone, Serialize)]
pub struct ItemNote {
    pub ledger_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ApproveRunReport {
    pub approver_member_id: String,
    pub approved: Vec<String>,
    pub already_approved: Vec<String>,
    /// Rows where the caller is the payee. Never approved.
    pub skipped_payee: Vec<String>,
    pub skipped: Vec<ItemNote>,
}

/// POST /payouts/runs/{id}/approve (`payouts.approve`). Records the caller's approval on every
/// item of the run except rows where the caller is the payee (reported as skipped_payee).
pub async fn approve_run(
    pool: &SqlitePool,
    directory: &dyn Directory,
    principal: &Principal,
    run_id: &str,
) -> Result<ApproveRunReport, PayoutError> {
    principal.require(scopes::PAYOUTS_APPROVE)?;
    let view = get_run_unscoped(pool, principal, run_id).await?;
    if view.run.status != RUN_PREPARED {
        return Err(PayoutError::InvalidState(format!("run is '{}'", view.run.status)));
    }
    let mut report = ApproveRunReport {
        approver_member_id: principal.member_id.clone(),
        ..Default::default()
    };
    for item in &view.items {
        let Some(entry) = ledger::get(pool, &item.ledger_id).await? else {
            report.skipped.push(ItemNote { ledger_id: item.ledger_id.clone(), reason: "ledger entry not found".into() });
            continue;
        };
        if principal_is_payee(pool, directory, principal, &entry).await? {
            report.skipped_payee.push(entry.id.clone());
            continue;
        }
        match approvals::approve_row(pool, directory, principal, &entry.id).await {
            Ok(RowApproval::Recorded) => report.approved.push(entry.id.clone()),
            Ok(RowApproval::AlreadyApproved) => report.already_approved.push(entry.id.clone()),
            Err(PayoutError::PayeeCannotApprove) => report.skipped_payee.push(entry.id.clone()),
            Err(PayoutError::InvalidState(reason)) => report.skipped.push(ItemNote { ledger_id: entry.id.clone(), reason }),
            Err(e) => return Err(e),
        }
    }
    Ok(report)
}

async fn get_run_unscoped(pool: &SqlitePool, principal: &Principal, run_id: &str) -> Result<RunView, PayoutError> {
    let view = load_run(pool, run_id).await?.ok_or(PayoutError::NotFound("run"))?;
    if view.run.org_id != principal.org_id {
        return Err(PayoutError::NotFound("run"));
    }
    Ok(view)
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecuteItem {
    pub ledger_id: String,
    /// paid | failed | already_paid | not_payable
    pub outcome: String,
    pub reason: Option<String>,
    pub stripe_transfer_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecuteReport {
    pub run_id: String,
    pub policy_version: Option<i64>,
    pub paid_cents: i64,
    pub items: Vec<ExecuteItem>,
    pub run_status: String,
}

async fn record_outcome(pool: &SqlitePool, run_id: &str, item: &ExecuteItem, now: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE payout_run_items SET outcome = ?, outcome_reason = ?, executed_at = ? WHERE run_id = ? AND ledger_id = ?")
        .bind(&item.outcome)
        .bind(&item.reason)
        .bind(now)
        .bind(run_id)
        .bind(&item.ledger_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// POST /payouts/runs/{id}/execute (`payouts.execute`). Pays every item that is payable right
/// now under the policy in force now, through the existing transfer path; limits are re-checked
/// inside the claim. Safe to call twice: a paid row cannot be claimed again.
pub async fn execute_run(
    pool: &SqlitePool,
    stripe: &dyn StripeApi,
    cfg: &PaymentConfig,
    directory: &dyn Directory,
    principal: &Principal,
    run_id: &str,
    now: DateTime<Utc>,
) -> Result<ExecuteReport, PayoutError> {
    principal.require(scopes::PAYOUTS_EXECUTE)?;
    let view = get_run_unscoped(pool, principal, run_id).await?;
    if view.run.status == RUN_CANCELLED {
        return Err(PayoutError::InvalidState("run is 'cancelled'".into()));
    }
    let eff = policy::load_effective(pool, &principal.org_id, cfg).await?;
    let p = &eff.policy;
    let now_s = format_db_time(now);
    let (start, end) = period_window(&p.schedule, now);
    let (start, end) = (format_db_time(start), format_db_time(end));

    // Amounts this run already committed (rows in flight or paid).
    let run_used: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(l.amount_cents), 0) FROM payout_run_items i JOIN payout_ledger l ON l.id = i.ledger_id
         WHERE i.run_id = ? AND l.status IN ('approved', 'paid')",
    )
    .bind(run_id)
    .fetch_one(pool)
    .await?;
    let mut usage = Usage { run_total: run_used, ..Default::default() };

    let mut items = Vec::new();
    let mut paid_cents = 0;
    for item in &view.items {
        let Some(entry) = ledger::get(pool, &item.ledger_id).await? else {
            continue;
        };
        if entry.status == PAID {
            let it = ExecuteItem {
                ledger_id: entry.id.clone(),
                outcome: "already_paid".into(),
                reason: None,
                stripe_transfer_id: entry.stripe_transfer_id.clone(),
            };
            items.push(it);
            continue;
        }
        let key = payee_key(&entry);
        if !usage.member_period.contains_key(&key) {
            let used = member_period_usage(pool, &key, &start, &end).await?;
            usage.member_period.insert(key.clone(), used);
        }
        let facts = load_facts(pool, directory, entry).await?;
        let blockers = evaluate(p, &facts, now, &usage);
        let entry = facts.entry;
        let it = if !blockers.is_empty() {
            ExecuteItem {
                ledger_id: entry.id.clone(),
                outcome: "not_payable".into(),
                reason: Some(blockers.iter().map(Blocker::message).collect::<Vec<_>>().join("; ")),
                stripe_transfer_id: None,
            }
        } else {
            let guard = claim_guard(p, now, Some(run_id));
            match ledger::pay_guarded(pool, stripe, &entry.id, &principal.member_id, &guard).await {
                Ok(done) if done.status == PAID => {
                    usage.add(&key, done.amount_cents);
                    paid_cents += done.amount_cents;
                    ExecuteItem {
                        ledger_id: done.id.clone(),
                        outcome: "paid".into(),
                        reason: None,
                        stripe_transfer_id: done.stripe_transfer_id.clone(),
                    }
                }
                Ok(done) => ExecuteItem {
                    ledger_id: done.id.clone(),
                    outcome: "failed".into(),
                    reason: done.failure_reason.clone(),
                    stripe_transfer_id: None,
                },
                Err(e @ (LedgerError::NotPayable(_) | LedgerError::InvalidState(..))) => ExecuteItem {
                    ledger_id: entry.id.clone(),
                    outcome: "not_payable".into(),
                    reason: Some(e.to_string()),
                    stripe_transfer_id: None,
                },
                Err(e) => return Err(e.into()),
            }
        };
        record_outcome(pool, run_id, &it, &now_s).await?;
        items.push(it);
    }

    // Completed when no item is still open (every row paid, rejected, or otherwise closed).
    let open: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM payout_run_items i JOIN payout_ledger l ON l.id = i.ledger_id
         WHERE i.run_id = ? AND l.status NOT IN ('paid', 'rejected')",
    )
    .bind(run_id)
    .fetch_one(pool)
    .await?;
    let status = if open == 0 { RUN_COMPLETED } else { RUN_PREPARED };
    sqlx::query("UPDATE payout_runs SET status = ?, last_executed_by = ?, last_executed_at = ? WHERE id = ? AND status <> ?")
        .bind(status)
        .bind(&principal.member_id)
        .bind(&now_s)
        .bind(run_id)
        .bind(RUN_CANCELLED)
        .execute(pool)
        .await?;

    Ok(ExecuteReport {
        run_id: run_id.to_string(),
        policy_version: eff.version,
        paid_cents,
        items,
        run_status: status.to_string(),
    })
}

/// POST /payouts/runs/{id}/cancel (`payouts.prepare`). Approvals already recorded stay on their rows.
pub async fn cancel_run(pool: &SqlitePool, principal: &Principal, run_id: &str, now: DateTime<Utc>) -> Result<RunView, PayoutError> {
    principal.require(scopes::PAYOUTS_PREPARE)?;
    let view = get_run_unscoped(pool, principal, run_id).await?;
    let res = sqlx::query(
        "UPDATE payout_runs SET status = ?, cancelled_by = ?, cancelled_at = ? WHERE id = ? AND status = ?",
    )
    .bind(RUN_CANCELLED)
    .bind(&principal.member_id)
    .bind(format_db_time(now))
    .bind(run_id)
    .bind(RUN_PREPARED)
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(PayoutError::InvalidState(format!("run is '{}'", view.run.status)));
    }
    load_run(pool, run_id).await?.ok_or(PayoutError::NotFound("run"))
}

/// GET /payouts/ledger?status= (`payouts.read`).
pub async fn list_ledger(pool: &SqlitePool, principal: &Principal, status: Option<&str>) -> Result<Vec<LedgerEntry>, PayoutError> {
    principal.require(scopes::PAYOUTS_READ)?;
    Ok(ledger::list(pool, status).await?)
}
