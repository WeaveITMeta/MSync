//! Approvals, payee blocks and the payable rule (contract section 8).
//!
//! A row is payable when: policy enabled; approvals from distinct members >= approvals_required,
//! each with rank >= min_approver_rank, none of them the payee; hold_days passed; eligibility met;
//! the payee is not blocked; every limit holds.

use super::config::PaymentConfig;
use super::error::PayoutError;
use super::ledger::{self, ClaimGuard, LedgerEntry, FAILED, PENDING};
use super::policy::{self, PayoutPolicy};
use super::schedule::period_window;
use super::stripe::StripeApi;
use crate::auth::{is_payee, scopes, tokens::format_db_time, tokens::parse_db_time, Directory, Principal, LEGACY_ADMIN_MEMBER};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use sqlx::SqlitePool;
use std::collections::HashMap;

/// The payee key of a ledger row: its github_username, lowercased.
pub fn payee_key(entry: &LedgerEntry) -> String {
    entry.github_username.to_lowercase()
}

fn is_open(status: &str) -> bool {
    status == PENDING || status == FAILED
}

// ---------------------------------------------------------------- approvals

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RowApproval {
    Recorded,
    AlreadyApproved,
}

async fn insert_approval(pool: &SqlitePool, ledger_id: &str, member_id: &str, token_id: &str, org: &str) -> Result<RowApproval, sqlx::Error> {
    let res = sqlx::query(
        "INSERT OR IGNORE INTO payout_approvals (ledger_id, approver_member_id, org_id, token_id) VALUES (?, ?, ?, ?)",
    )
    .bind(ledger_id)
    .bind(member_id)
    .bind(org)
    .bind(token_id)
    .execute(pool)
    .await?;
    Ok(if res.rows_affected() == 0 { RowApproval::AlreadyApproved } else { RowApproval::Recorded })
}

/// Is the principal the payee of this row? Checks the token's own payee keys and every payee key
/// the member has ever declared (all tokens), plus the directory.
pub async fn principal_is_payee(
    pool: &SqlitePool,
    directory: &dyn Directory,
    principal: &Principal,
    entry: &LedgerEntry,
) -> Result<bool, sqlx::Error> {
    let key = payee_key(entry);
    if principal.payee_keys.iter().any(|k| k.to_lowercase() == key) {
        return Ok(true);
    }
    if row_payee_member(pool, entry).await?.as_deref() == Some(principal.member_id.as_str()) {
        return Ok(true);
    }
    is_payee(pool, directory, &principal.member_id, &key).await
}

/// The CRA member id recorded on the payee record of a pay order or invoice row, if any.
async fn row_payee_member(pool: &SqlitePool, entry: &LedgerEntry) -> Result<Option<String>, sqlx::Error> {
    let Some(pid) = &entry.payee_id else { return Ok(None) };
    let m: Option<Option<String>> = sqlx::query_scalar("SELECT member_id FROM payees WHERE id = ?")
        .bind(pid)
        .fetch_optional(pool)
        .await?;
    Ok(m.flatten())
}

/// Record the principal's approval of one ledger row (`payouts.approve`). The approver is the
/// token's member. A payee can never approve their own row. One member approving twice counts once.
pub async fn approve_row(
    pool: &SqlitePool,
    directory: &dyn Directory,
    principal: &Principal,
    ledger_id: &str,
) -> Result<RowApproval, PayoutError> {
    principal.require(scopes::PAYOUTS_APPROVE)?;
    let entry = ledger::get(pool, ledger_id).await?.ok_or(PayoutError::NotFound("ledger entry"))?;
    if principal_is_payee(pool, directory, principal, &entry).await? {
        return Err(PayoutError::PayeeCannotApprove);
    }
    if !is_open(&entry.status) {
        return Err(PayoutError::InvalidState(format!("ledger entry is '{}' and cannot be approved", entry.status)));
    }
    Ok(insert_approval(pool, &entry.id, &principal.member_id, &principal.token_id, &principal.org_id).await?)
}

#[derive(Debug, Clone, Serialize)]
pub struct ApprovalView {
    pub approver_member_id: String,
    pub created_at: String,
}

pub async fn approvals_for(pool: &SqlitePool, ledger_id: &str) -> Result<Vec<ApprovalView>, sqlx::Error> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT approver_member_id, created_at FROM payout_approvals WHERE ledger_id = ? ORDER BY created_at, approver_member_id",
    )
    .bind(ledger_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(approver_member_id, created_at)| ApprovalView { approver_member_id, created_at })
        .collect())
}

// ---------------------------------------------------------------- blocks

/// Block a payee (`payouts.policy`). A blocked payee is never paid.
pub async fn block_payee(pool: &SqlitePool, principal: &Principal, payee: &str, reason: &str) -> Result<(), PayoutError> {
    principal.require(scopes::PAYOUTS_POLICY)?;
    let key = payee.trim().to_lowercase();
    if key.is_empty() || reason.trim().is_empty() {
        return Err(PayoutError::Invalid(vec!["payee_key and reason are required".into()]));
    }
    sqlx::query(
        "INSERT INTO payout_blocks (payee_key, org_id, reason, created_by) VALUES (?, ?, ?, ?)
         ON CONFLICT(payee_key) DO UPDATE SET reason = excluded.reason, created_by = excluded.created_by, created_at = datetime('now')",
    )
    .bind(&key)
    .bind(&principal.org_id)
    .bind(reason.trim())
    .bind(&principal.member_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Remove a block (`payouts.policy`). Returns whether a block existed.
pub async fn unblock_payee(pool: &SqlitePool, principal: &Principal, payee: &str) -> Result<bool, PayoutError> {
    principal.require(scopes::PAYOUTS_POLICY)?;
    let res = sqlx::query("DELETE FROM payout_blocks WHERE payee_key = ?")
        .bind(payee.trim().to_lowercase())
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn is_blocked(pool: &SqlitePool, payee: &str) -> Result<bool, sqlx::Error> {
    let hit: Option<i64> = sqlx::query_scalar("SELECT 1 FROM payout_blocks WHERE payee_key = ?")
        .bind(payee.to_lowercase())
        .fetch_optional(pool)
        .await?;
    Ok(hit.is_some())
}

// ---------------------------------------------------------------- the payable rule

/// One approver of a row, with what the rule needs to know about them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ApproverFact {
    pub member_id: String,
    pub rank: Option<i64>,
    pub is_payee: bool,
}

/// Everything the payable rule reads about one ledger row.
#[derive(Debug, Clone)]
pub struct RowFacts {
    pub entry: LedgerEntry,
    pub approvers: Vec<ApproverFact>,
    pub blocked: bool,
    pub verified_identity: bool,
    pub payee_rank: Option<i64>,
    pub payee_roles: Vec<String>,
    /// Some(reason) when a payee record pays this row (by payee id, or by payee key for merit
    /// rows) and it is paused or closed, or (pay order and invoice rows) its Stripe account
    /// cannot receive transfers. None for merit rows with no payee record (contract 11.2).
    pub payee_inactive: Option<String>,
}

/// Amounts already committed against the limits: per payee in the current period, and in this run.
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub member_period: HashMap<String, i64>,
    pub run_total: i64,
}

impl Usage {
    pub fn add(&mut self, payee: &str, amount: i64) {
        *self.member_period.entry(payee.to_string()).or_insert(0) += amount;
        self.run_total += amount;
    }
}

/// Why a row is not payable right now.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum Blocker {
    PolicyDisabled,
    NotOpen { status: String },
    ZeroAmount,
    CurrencyMismatch { row: String, policy: String },
    Hold { until: String },
    PayeeBlocked,
    Ineligible { reason: String },
    Approvals { have: i64, need: i64 },
    BelowMinimum { min: i64 },
    AboveMaxPerPayout { max: i64 },
    MemberPeriodLimit { used: i64, max: i64 },
    RunLimit { used: i64, max: i64 },
    /// The row's pay_after time (occurrence date or invoice due date) has not come yet.
    NotYet { until: String },
    /// The payee record is paused or closed, or cannot receive transfers.
    PayeeInactive { reason: String },
}

impl Blocker {
    /// Rows blocked only by missing approvals are still included in a prepared run.
    pub fn is_approvals(&self) -> bool {
        matches!(self, Blocker::Approvals { .. })
    }

    pub fn message(&self) -> String {
        match self {
            Blocker::PolicyDisabled => "payouts are disabled by policy".into(),
            Blocker::NotOpen { status } => format!("ledger entry is '{status}'"),
            Blocker::ZeroAmount => "amount is zero".into(),
            Blocker::CurrencyMismatch { row, policy } => format!("row currency {row} differs from policy currency {policy}"),
            Blocker::Hold { until } => format!("on hold until {until}"),
            Blocker::PayeeBlocked => "payee is blocked".into(),
            Blocker::Ineligible { reason } => format!("payee not eligible: {reason}"),
            Blocker::Approvals { have, need } => format!("{have} of {need} required approvals"),
            Blocker::BelowMinimum { min } => format!("amount below min_payout_cents {min}"),
            Blocker::AboveMaxPerPayout { max } => format!("amount above max_per_payout_cents {max}"),
            Blocker::MemberPeriodLimit { used, max } => format!("payee period total {used} + amount exceeds max_per_member_per_period_cents {max}"),
            Blocker::RunLimit { used, max } => format!("run total {used} + amount exceeds max_per_run_cents {max}"),
            Blocker::NotYet { until } => format!("not payable before {until}"),
            Blocker::PayeeInactive { reason } => format!("payee {reason}"),
        }
    }
}

/// Distinct approvers that count: not the payee, and rank >= min_approver_rank (an unranked
/// approver counts only when min_approver_rank is 0).
pub fn counted_approvals(policy: &PayoutPolicy, approvers: &[ApproverFact]) -> i64 {
    let min = policy.approval.min_approver_rank;
    let mut seen: Vec<&str> = Vec::new();
    for a in approvers {
        if a.is_payee || seen.contains(&a.member_id.as_str()) {
            continue;
        }
        let rank_ok = match a.rank {
            Some(r) => r >= min,
            None => min <= 0,
        };
        if rank_ok {
            seen.push(&a.member_id);
        }
    }
    seen.len() as i64
}

/// The payable rule, pure. Empty result = payable now.
pub fn evaluate(policy: &PayoutPolicy, facts: &RowFacts, now: DateTime<Utc>, usage: &Usage) -> Vec<Blocker> {
    let e = &facts.entry;
    let mut out = Vec::new();
    if !policy.enabled {
        out.push(Blocker::PolicyDisabled);
    }
    if !is_open(&e.status) {
        out.push(Blocker::NotOpen { status: e.status.clone() });
    }
    if e.amount_cents <= 0 {
        out.push(Blocker::ZeroAmount);
    }
    if !e.currency.eq_ignore_ascii_case(&policy.currency) {
        out.push(Blocker::CurrencyMismatch { row: e.currency.clone(), policy: policy.currency.clone() });
    }
    let hold = policy.hold_days();
    if hold > 0 {
        match parse_db_time(&e.created_at) {
            Some(created) => {
                let until = created + Duration::days(hold);
                if now < until {
                    out.push(Blocker::Hold { until: format_db_time(until) });
                }
            }
            // Unknown merge time: fail closed.
            None => out.push(Blocker::Hold { until: "unknown".into() }),
        }
    }
    if let Some(after) = &e.pay_after {
        match parse_db_time(after) {
            Some(t) if now < t => out.push(Blocker::NotYet { until: after.clone() }),
            Some(_) => {}
            // Unreadable time: fail closed.
            None => out.push(Blocker::NotYet { until: "unknown".into() }),
        }
    }
    if facts.blocked {
        out.push(Blocker::PayeeBlocked);
    }
    if let Some(reason) = &facts.payee_inactive {
        out.push(Blocker::PayeeInactive { reason: reason.clone() });
    }

    let el = &policy.eligibility;
    if el.min_rank > 0 && facts.payee_rank.map_or(true, |r| r < el.min_rank) {
        out.push(Blocker::Ineligible { reason: format!("payee rank below {}", el.min_rank) });
    }
    if !el.roles.is_empty() && !facts.payee_roles.iter().any(|r| el.roles.contains(r)) {
        out.push(Blocker::Ineligible { reason: "payee holds none of the eligible roles".into() });
    }
    if el.require_verified_identity && !facts.verified_identity {
        out.push(Blocker::Ineligible { reason: "payee identity not verified".into() });
    }

    let have = counted_approvals(policy, &facts.approvers);
    let need = policy.approval.approvals_required.max(1);
    if have < need {
        out.push(Blocker::Approvals { have, need });
    }

    let l = &policy.limits;
    let amt = e.amount_cents;
    if let Some(min) = l.min_payout_cents {
        if amt < min {
            out.push(Blocker::BelowMinimum { min });
        }
    }
    if let Some(max) = l.max_per_payout_cents {
        if amt > max {
            out.push(Blocker::AboveMaxPerPayout { max });
        }
    }
    if let Some(max) = l.max_per_member_per_period_cents {
        let used = *usage.member_period.get(&payee_key(e)).unwrap_or(&0);
        if used + amt > max {
            out.push(Blocker::MemberPeriodLimit { used, max });
        }
    }
    if let Some(max) = l.max_per_run_cents {
        if usage.run_total + amt > max {
            out.push(Blocker::RunLimit { used: usage.run_total, max });
        }
    }
    out
}

/// Load the facts the payable rule needs for one row.
pub async fn load_facts(pool: &SqlitePool, directory: &dyn Directory, entry: LedgerEntry) -> Result<RowFacts, sqlx::Error> {
    let key = payee_key(&entry);
    let payee_member = row_payee_member(pool, &entry).await?;
    let mut approvers = Vec::new();
    for a in approvals_for(pool, &entry.id).await? {
        let payee = payee_member.as_deref() == Some(a.approver_member_id.as_str())
            || is_payee(pool, directory, &a.approver_member_id, &key).await?;
        approvers.push(ApproverFact {
            rank: directory.rank_of(&a.approver_member_id),
            member_id: a.approver_member_id,
            is_payee: payee,
        });
    }
    let mut blocked = is_blocked(pool, &key).await?;
    if let Some(pid) = &entry.payee_id {
        // A block on any of the payee's keys blocks its pay order and invoice rows too.
        let hit: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM payout_blocks b JOIN payee_keys k ON k.payee_key = b.payee_key WHERE k.payee_id = ? LIMIT 1",
        )
        .bind(pid)
        .fetch_optional(pool)
        .await?;
        blocked = blocked || hit.is_some();
    }
    let payee_inactive = payee_inactive_reason(pool, &entry).await?;
    let verified: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM contributors WHERE lower(github_username) = ? AND discord_user_id IS NOT NULL",
    )
    .bind(&key)
    .fetch_optional(pool)
    .await?;
    let payee_member = directory.member_for_payee(&key).or(payee_member);
    Ok(RowFacts {
        approvers,
        blocked,
        verified_identity: verified.is_some(),
        payee_rank: payee_member.as_deref().and_then(|m| directory.rank_of(m)),
        payee_roles: payee_member.as_deref().map(|m| directory.roles_of(m)).unwrap_or_default(),
        payee_inactive,
        entry,
    })
}

/// Why the payee record behind a row cannot be paid now, if it cannot.
async fn payee_inactive_reason(pool: &SqlitePool, entry: &LedgerEntry) -> Result<Option<String>, sqlx::Error> {
    let status: Option<String> = sqlx::query_scalar(
        "SELECT p.status FROM payees p WHERE p.status <> 'active' AND (p.id = ? OR p.id IN
         (SELECT k.payee_id FROM payee_keys k WHERE k.payee_key = lower(?))) LIMIT 1",
    )
    .bind(&entry.payee_id)
    .bind(&entry.github_username)
    .fetch_optional(pool)
    .await?;
    if let Some(st) = status {
        return Ok(Some(format!("is {st}")));
    }
    if let Some(pid) = &entry.payee_id {
        let acct: Option<(Option<String>, i64)> =
            sqlx::query_as("SELECT stripe_account_id, stripe_payouts_enabled FROM payees WHERE id = ?")
                .bind(pid)
                .fetch_optional(pool)
                .await?;
        return Ok(match acct {
            None => Some("record not found".into()),
            Some((None, _)) => Some("has no Stripe account".into()),
            Some((Some(a), _)) if a.is_empty() => Some("has no Stripe account".into()),
            Some((Some(_), 0)) => Some("Stripe account cannot receive transfers yet".into()),
            Some(_) => None,
        });
    }
    Ok(None)
}

/// Sum of this payee's approved/paid rows whose updated_at falls in [start, end).
pub async fn member_period_usage(pool: &SqlitePool, payee: &str, start: &str, end: &str) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount_cents), 0) FROM payout_ledger
         WHERE lower(github_username) = ? AND status IN ('approved', 'paid') AND updated_at >= ? AND updated_at < ?",
    )
    .bind(payee.to_lowercase())
    .bind(start)
    .bind(end)
    .fetch_one(pool)
    .await
}

/// The claim guard that re-checks the policy's limits inside the atomic claim.
pub fn claim_guard(policy: &PayoutPolicy, now: DateTime<Utc>, run_id: Option<&str>) -> ClaimGuard {
    let (start, end) = period_window(&policy.schedule, now);
    let l = &policy.limits;
    ClaimGuard {
        min_cents: l.min_payout_cents,
        max_cents: l.max_per_payout_cents,
        member_period: l
            .max_per_member_per_period_cents
            .map(|cap| (format_db_time(start), format_db_time(end), cap)),
        run_cap: match (run_id, l.max_per_run_cents) {
            (Some(r), Some(cap)) => Some((r.to_string(), cap)),
            _ => None,
        },
        check_blocks: true,
        now: Some(format_db_time(now)),
    }
}

// ---------------------------------------------------------------- legacy admin route

/// Result of the legacy `/api/admin/ledger/{id}/approve` under the policy.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum LegacyApproveOutcome {
    /// The transfer path ran (the entry is paid, or failed at Stripe).
    Transferred { entry: LedgerEntry },
    /// The admin-token approval was recorded but the row is not payable through this route.
    ApprovalRecorded { entry: LedgerEntry, blockers: Vec<String> },
}

/// The legacy admin approve. With no stored policy: exactly `ledger::approve` (today's behaviour,
/// `decided_by` from the body). With a stored policy: one approval by `admin-token`, and pay only
/// when the policy allows a single approver and every other payable check passes.
pub async fn legacy_admin_approve(
    pool: &SqlitePool,
    stripe: &dyn StripeApi,
    cfg: &PaymentConfig,
    directory: &dyn Directory,
    ledger_id: &str,
    decided_by: &str,
    now: DateTime<Utc>,
) -> Result<LegacyApproveOutcome, PayoutError> {
    let eff = policy::load_effective(pool, crate::auth::DEFAULT_ORG, cfg).await?;
    if !eff.is_stored() {
        let entry = ledger::approve(pool, stripe, ledger_id, decided_by).await?;
        return Ok(LegacyApproveOutcome::Transferred { entry });
    }
    let p = &eff.policy;
    let entry = ledger::get(pool, ledger_id).await?.ok_or(PayoutError::NotFound("ledger entry"))?;
    if !is_open(&entry.status) {
        return Err(PayoutError::Ledger(ledger::LedgerError::InvalidState(entry.status, "approved")));
    }
    let admin = Principal::legacy_admin();
    insert_approval(pool, &entry.id, LEGACY_ADMIN_MEMBER, &admin.token_id, &admin.org_id).await?;

    let mut blockers: Vec<String> = Vec::new();
    if p.approval.approvals_required != 1 {
        blockers.push(format!(
            "policy requires {} approvals; the legacy route pays only under a single-approver policy",
            p.approval.approvals_required
        ));
    }
    let (start, end) = period_window(&p.schedule, now);
    let mut usage = Usage::default();
    let key = payee_key(&entry);
    usage.member_period.insert(
        key.clone(),
        member_period_usage(pool, &key, &format_db_time(start), &format_db_time(end)).await?,
    );
    let facts = load_facts(pool, directory, entry.clone()).await?;
    // Not a run: the per-run cap does not apply here.
    let mut no_run = p.clone();
    no_run.limits.max_per_run_cents = None;
    blockers.extend(evaluate(&no_run, &facts, now, &usage).iter().map(Blocker::message));
    if !blockers.is_empty() {
        return Ok(LegacyApproveOutcome::ApprovalRecorded { entry, blockers });
    }
    let entry = ledger::pay_guarded(pool, stripe, ledger_id, LEGACY_ADMIN_MEMBER, &claim_guard(p, now, None)).await?;
    Ok(LegacyApproveOutcome::Transferred { entry })
}
