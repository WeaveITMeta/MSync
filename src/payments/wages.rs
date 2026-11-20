//! Hourly wages (contract 14.2): a rate history per payee.
//!
//! A rate is cents per hour, a currency and an effective-from date. A new rate is a new row;
//! history is kept and nothing is edited. Setting a rate follows the pay order authority
//! rules (contract 11.5): the person who sets it never approves it, a payee never sets or
//! approves their own, and a rate pays nothing until it has the payout policy's required
//! approvals from distinct members at or above `min_approver_rank`. Approvals are counted
//! again every time pay is computed, exactly as for pay orders.
//!
//! Optional overtime: minutes beyond `overtime_after_minutes` in one week (Monday to Sunday,
//! in the shift's time zone) are paid at `overtime_bps / 10000` times the rate.

use super::config::PaymentConfig;
use super::error::PayoutError;
use super::payees;
use super::payorders::{member_is_payee_of, OrderApproval};
use super::policy::{self, PayoutPolicy};
use crate::auth::{scopes, tokens::format_db_time, Directory, Principal};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::SqlitePool;
use std::collections::HashSet;
use uuid::Uuid;

/// Highest rate accepted: 10,000,000 cents ($100,000) per hour.
pub const MAX_RATE_CENTS_PER_HOUR: i64 = 10_000_000;
/// Overtime multiplier range: 1x to 5x.
pub const MIN_OVERTIME_BPS: i64 = 10_000;
pub const MAX_OVERTIME_BPS: i64 = 50_000;
pub const MAX_OVERTIME_AFTER_MINUTES: i64 = 7 * 24 * 60;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct WageRow {
    pub id: String,
    pub org_id: String,
    pub payee_id: String,
    pub rate_cents_per_hour: i64,
    pub currency: String,
    pub effective_from: String,
    pub overtime_after_minutes: Option<i64>,
    pub overtime_bps: Option<i64>,
    pub memo: Option<String>,
    pub set_by: String,
    pub created_at: String,
}

/// A rate as the pure pay functions see it (approved rows only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WageRate {
    pub id: String,
    pub effective_from: NaiveDate,
    pub created_at: String,
    pub rate_cents_per_hour: i64,
    pub overtime_after_minutes: Option<i64>,
    pub overtime_bps: Option<i64>,
}

/// Pure: the rate in force on `day`: the newest of `rates` (approved ones only) whose
/// effective_from is on or before `day`. Ties on the date go to the later-created row.
pub fn rate_on(rates: &[WageRate], day: NaiveDate) -> Option<&WageRate> {
    rates
        .iter()
        .filter(|r| r.effective_from <= day)
        .max_by(|a, b| (a.effective_from, &a.created_at, &a.id).cmp(&(b.effective_from, &b.created_at, &b.id)))
}

#[derive(Debug, Clone, Serialize)]
pub struct WageView {
    #[serde(flatten)]
    pub row: WageRow,
    pub approvals: Vec<OrderApproval>,
    pub approvals_counted: i64,
    pub approvals_required: i64,
    /// Has the approvals to pay. An unapproved rate pays nothing.
    pub approved: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WageHistory {
    pub payee_id: String,
    /// The approved rate in force today (UTC date), if any.
    pub in_force: Option<WageView>,
    /// Newest first.
    pub wages: Vec<WageView>,
}

/// POST /api/v1/payees/{id}/wage. The payee comes from the path; the setter is the token's
/// member.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewWage {
    pub rate_cents_per_hour: i64,
    /// YYYY-MM-DD: the first day (in the shift's time zone) this rate pays.
    pub effective_from: NaiveDate,
    /// Defaults to the payout policy's currency; must equal it.
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub overtime_after_minutes: Option<i64>,
    #[serde(default)]
    pub overtime_bps: Option<i64>,
    #[serde(default)]
    pub memo: Option<String>,
}

impl NewWage {
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if !(0..=MAX_RATE_CENTS_PER_HOUR).contains(&self.rate_cents_per_hour) {
            errs.push(format!("rate_cents_per_hour must be between 0 and {MAX_RATE_CENTS_PER_HOUR}"));
        }
        if !(2000..=2100).contains(&chrono::Datelike::year(&self.effective_from)) {
            errs.push("effective_from must be between 2000 and 2100".into());
        }
        match (self.overtime_after_minutes, self.overtime_bps) {
            (None, None) => {}
            (Some(m), Some(b)) => {
                if !(0..=MAX_OVERTIME_AFTER_MINUTES).contains(&m) {
                    errs.push(format!("overtime_after_minutes must be between 0 and {MAX_OVERTIME_AFTER_MINUTES}"));
                }
                if !(MIN_OVERTIME_BPS..=MAX_OVERTIME_BPS).contains(&b) {
                    errs.push(format!("overtime_bps must be between {MIN_OVERTIME_BPS} (1x) and {MAX_OVERTIME_BPS} (5x)"));
                }
            }
            _ => errs.push("overtime_after_minutes and overtime_bps are set together, or not at all".into()),
        }
        if self.memo.as_deref().is_some_and(|m| m.chars().count() > 500) {
            errs.push("memo must be at most 500 characters".into());
        }
        errs
    }
}

fn required(policy: &PayoutPolicy) -> i64 {
    policy.approval.approvals_required.max(1)
}

async fn load_row(pool: &SqlitePool, id: &str) -> Result<Option<WageRow>, sqlx::Error> {
    sqlx::query_as::<_, WageRow>("SELECT * FROM payee_wages WHERE id = ?").bind(id).fetch_optional(pool).await
}

/// The approvals of one rate that satisfy the policy: distinct, not the setter, not the payee,
/// rank >= min_approver_rank.
pub async fn wage_approvals(pool: &SqlitePool, directory: &dyn Directory, policy: &PayoutPolicy, row: &WageRow) -> Result<(Vec<OrderApproval>, i64), sqlx::Error> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT approver_member_id, created_at FROM payee_wage_approvals WHERE wage_id = ? ORDER BY created_at, approver_member_id")
            .bind(&row.id)
            .fetch_all(pool)
            .await?;
    let min = policy.approval.min_approver_rank;
    let ids = [row.payee_id.clone()];
    let mut out = Vec::new();
    let mut counted: HashSet<String> = HashSet::new();
    for (m, at) in rows {
        let rank_ok = match directory.rank_of(&m) {
            Some(r) => r >= min,
            None => min <= 0,
        };
        let reason = if m == row.set_by && !super::mutant::on("wage_setter_counts") {
            Some("the member who set a rate never approves it".to_string())
        } else if member_is_payee_of(pool, directory, &m, &[], Some(&ids)).await? {
            Some("the payee never approves their own rate".to_string())
        } else if !rank_ok {
            Some(format!("rank below min_approver_rank {min}"))
        } else if counted.contains(&m) {
            Some("duplicate".to_string())
        } else {
            None
        };
        if reason.is_none() {
            counted.insert(m.clone());
        }
        out.push(OrderApproval { approver_member_id: m, created_at: at, counts: reason.is_none(), reason });
    }
    let n = counted.len() as i64;
    Ok((out, n))
}

async fn view(pool: &SqlitePool, directory: &dyn Directory, policy: &PayoutPolicy, row: WageRow) -> Result<WageView, sqlx::Error> {
    let (approvals, counted) = wage_approvals(pool, directory, policy, &row).await?;
    let need = required(policy);
    Ok(WageView { row, approvals, approvals_counted: counted, approvals_required: need, approved: counted >= need })
}

/// The approved rates of a payee, for the pay functions. Approvals are counted now.
pub async fn approved_rates(pool: &SqlitePool, directory: &dyn Directory, policy: &PayoutPolicy, payee_id: &str) -> Result<Vec<WageRate>, sqlx::Error> {
    let rows = sqlx::query_as::<_, WageRow>("SELECT * FROM payee_wages WHERE payee_id = ? ORDER BY effective_from, created_at, id")
        .bind(payee_id)
        .fetch_all(pool)
        .await?;
    let mut out = Vec::new();
    for r in rows {
        let (_, counted) = wage_approvals(pool, directory, policy, &r).await?;
        if counted < required(policy) && !super::mutant::on("unapproved_wage_pays") {
            continue;
        }
        let Ok(effective_from) = NaiveDate::parse_from_str(&r.effective_from, "%Y-%m-%d") else {
            tracing::error!("wage {} has an unreadable effective_from; it pays nothing", r.id);
            continue;
        };
        out.push(WageRate {
            id: r.id,
            effective_from,
            created_at: r.created_at,
            rate_cents_per_hour: r.rate_cents_per_hour,
            overtime_after_minutes: r.overtime_after_minutes,
            overtime_bps: r.overtime_bps,
        });
    }
    Ok(out)
}

async fn scoped_payee(pool: &SqlitePool, principal: &Principal, payee_id: &str) -> Result<payees::Payee, PayoutError> {
    let p = payees::load(pool, payee_id).await?.ok_or(PayoutError::NotFound("payee"))?;
    if p.row.org_id != principal.org_id {
        return Err(PayoutError::NotFound("payee"));
    }
    Ok(p)
}

/// GET /api/v1/payees/{id}/wage (`payees.read`): the rate history and the rate in force.
pub async fn history(
    pool: &SqlitePool,
    cfg: &PaymentConfig,
    directory: &dyn Directory,
    principal: &Principal,
    payee_id: &str,
    now: DateTime<Utc>,
) -> Result<WageHistory, PayoutError> {
    super::timeclock::need(principal, scopes::PAYEES_READ)?;
    let payee = scoped_payee(pool, principal, payee_id).await?;
    let eff = policy::load_effective(pool, &payee.row.org_id, cfg).await?;
    let rows = sqlx::query_as::<_, WageRow>("SELECT * FROM payee_wages WHERE payee_id = ? ORDER BY effective_from DESC, created_at DESC, id DESC")
        .bind(&payee.row.id)
        .fetch_all(pool)
        .await?;
    let mut wages = Vec::new();
    for r in rows {
        wages.push(view(pool, directory, &eff.policy, r).await?);
    }
    let rates = approved_rates(pool, directory, &eff.policy, &payee.row.id).await?;
    let in_force = rate_on(&rates, now.date_naive()).and_then(|r| wages.iter().find(|w| w.row.id == r.id).cloned());
    Ok(WageHistory { payee_id: payee.row.id, in_force, wages })
}

/// POST /api/v1/payees/{id}/wage (`payees.manage`). A new rate row, waiting for approvals.
/// A payee can never set their own rate. Audited in the same transaction.
pub async fn set(
    pool: &SqlitePool,
    cfg: &PaymentConfig,
    directory: &dyn Directory,
    principal: &Principal,
    payee_id: &str,
    input: NewWage,
    now: DateTime<Utc>,
) -> Result<WageView, PayoutError> {
    super::timeclock::need(principal, scopes::PAYEES_MANAGE)?;
    let payee = scoped_payee(pool, principal, payee_id).await?;
    if payees::principal_is_payee_of(pool, directory, principal, &payee.row.id).await? && !super::mutant::on("payee_sets_own_wage") {
        return Err(PayoutError::Forbidden("a payee cannot set their own rate".into()));
    }
    if payee.row.status == "closed" {
        return Err(PayoutError::InvalidState("the payee is closed".into()));
    }
    let errs = input.validate();
    if !errs.is_empty() {
        return Err(PayoutError::Invalid(errs));
    }
    let eff = policy::load_effective(pool, &payee.row.org_id, cfg).await?;
    let currency = input.currency.as_deref().map(|c| c.trim().to_lowercase()).unwrap_or_else(|| eff.policy.currency.to_lowercase());
    if !currency.eq_ignore_ascii_case(&eff.policy.currency) {
        return Err(PayoutError::Invalid(vec![format!("currency must be the payout policy currency {}", eff.policy.currency)]));
    }
    let row = WageRow {
        id: Uuid::new_v4().to_string(),
        org_id: payee.row.org_id.clone(),
        payee_id: payee.row.id.clone(),
        rate_cents_per_hour: input.rate_cents_per_hour,
        currency,
        effective_from: input.effective_from.format("%Y-%m-%d").to_string(),
        overtime_after_minutes: input.overtime_after_minutes,
        overtime_bps: input.overtime_bps,
        memo: input.memo.as_deref().map(str::trim).filter(|m| !m.is_empty()).map(str::to_string),
        set_by: principal.member_id.clone(),
        created_at: format_db_time(now),
    };
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO payee_wages (id, org_id, payee_id, rate_cents_per_hour, currency, effective_from, overtime_after_minutes,
         overtime_bps, memo, set_by, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&row.id)
    .bind(&row.org_id)
    .bind(&row.payee_id)
    .bind(row.rate_cents_per_hour)
    .bind(&row.currency)
    .bind(&row.effective_from)
    .bind(row.overtime_after_minutes)
    .bind(row.overtime_bps)
    .bind(&row.memo)
    .bind(&row.set_by)
    .bind(&row.created_at)
    .execute(&mut *tx)
    .await?;
    crate::audit::append_in(
        &mut tx,
        &crate::audit::AuditEntry::by_principal(
            principal,
            "wage.set",
            format!("payee:{}", row.payee_id),
            json!({
                "wage_id": row.id,
                "payee_id": row.payee_id,
                "rate_cents_per_hour": row.rate_cents_per_hour,
                "currency": row.currency,
                "effective_from": row.effective_from,
                "overtime_after_minutes": row.overtime_after_minutes,
                "overtime_bps": row.overtime_bps,
            }),
        ),
    )
    .await?;
    tx.commit().await?;
    Ok(view(pool, directory, &eff.policy, row).await?)
}

#[derive(Debug, Clone, Serialize)]
pub struct ApproveWageResult {
    /// recorded | already_approved
    pub result: &'static str,
    pub approver_member_id: String,
    pub wage: WageView,
}

/// POST /api/v1/payees/{id}/wage/{wage_id}/approve (`payorders.approve`, the same rule as a
/// pay order). Refused for the member who set the rate and for the payee. An approval below
/// min_approver_rank is recorded but does not count. Audited in the same transaction.
pub async fn approve(
    pool: &SqlitePool,
    cfg: &PaymentConfig,
    directory: &dyn Directory,
    principal: &Principal,
    payee_id: &str,
    wage_id: &str,
    now: DateTime<Utc>,
) -> Result<ApproveWageResult, PayoutError> {
    super::timeclock::need(principal, scopes::PAYORDERS_APPROVE)?;
    let payee = scoped_payee(pool, principal, payee_id).await?;
    let row = load_row(pool, wage_id).await?.ok_or(PayoutError::NotFound("wage"))?;
    if row.payee_id != payee.row.id {
        return Err(PayoutError::NotFound("wage"));
    }
    if principal.member_id == row.set_by && !super::mutant::on("wage_setter_counts") {
        return Err(PayoutError::Forbidden("the member who set a rate can never approve it".into()));
    }
    if payees::principal_is_payee_of(pool, directory, principal, &row.payee_id).await? && !super::mutant::on("payee_approves_own_wage") {
        return Err(PayoutError::Forbidden("a payee cannot approve their own rate".into()));
    }
    let mut tx = pool.begin().await?;
    let res = sqlx::query(
        "INSERT OR IGNORE INTO payee_wage_approvals (wage_id, approver_member_id, org_id, token_id, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&row.id)
    .bind(&principal.member_id)
    .bind(&principal.org_id)
    .bind(&principal.token_id)
    .bind(format_db_time(now))
    .execute(&mut *tx)
    .await?;
    let recorded = res.rows_affected() > 0;
    if recorded {
        crate::audit::append_in(
            &mut tx,
            &crate::audit::AuditEntry::by_principal(
                principal,
                "wage.approved",
                format!("payee:{}", row.payee_id),
                json!({ "wage_id": row.id, "payee_id": row.payee_id, "rate_cents_per_hour": row.rate_cents_per_hour, "effective_from": row.effective_from }),
            ),
        )
        .await?;
    }
    tx.commit().await?;
    let eff = policy::load_effective(pool, &row.org_id, cfg).await?;
    Ok(ApproveWageResult {
        result: if recorded { "recorded" } else { "already_approved" },
        approver_member_id: principal.member_id.clone(),
        wage: view(pool, directory, &eff.policy, row).await?,
    })
}
