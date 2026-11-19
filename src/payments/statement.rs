//! Payee statement (contract 11.6): everything paid or pending to a payee in a date range,
//! grouped by source, with totals.
//!
//! A payee's rows are its pay order and invoice rows (`payee_id`) and the merit rows paid under
//! any of its payee keys. The range is on the day a row was recorded (`created_at`), inclusive.
//! Paid = `paid`; pending = `pending_approval`, `approved` (in flight) or `failed` (will be
//! retried). Rejected rows are listed but never counted.

use super::error::PayoutError;
use super::ledger::{LedgerEntry, APPROVED, FAILED, PAID, PENDING, REJECTED};
use super::payees;
use crate::auth::{scopes, Principal};
use chrono::NaiveDate;
use serde::Serialize;
use sqlx::SqlitePool;
use std::collections::BTreeMap;

pub const MAX_RANGE_DAYS: i64 = 3700;

#[derive(Debug, Clone, Default, Serialize)]
pub struct SourceTotals {
    pub source_kind: String,
    pub paid_cents: i64,
    pub pending_cents: i64,
    pub rejected_cents: i64,
    pub rows: Vec<LedgerEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Statement {
    pub payee_id: String,
    pub display_name: String,
    pub from: String,
    pub to: String,
    pub currency: Option<String>,
    pub sources: Vec<SourceTotals>,
    pub paid_cents: i64,
    pub pending_cents: i64,
}

/// Pure: group rows by source and total them.
pub fn summarize(rows: Vec<LedgerEntry>) -> (Vec<SourceTotals>, i64, i64) {
    let mut by: BTreeMap<String, SourceTotals> = BTreeMap::new();
    for r in rows {
        let t = by.entry(r.source_kind.clone()).or_insert_with(|| SourceTotals { source_kind: r.source_kind.clone(), ..Default::default() });
        match r.status.as_str() {
            s if s == PAID => t.paid_cents += r.amount_cents,
            s if s == PENDING || s == APPROVED || s == FAILED => t.pending_cents += r.amount_cents,
            s if s == REJECTED => t.rejected_cents += r.amount_cents,
            _ => {}
        }
        t.rows.push(r);
    }
    let sources: Vec<SourceTotals> = by.into_values().collect();
    let paid = sources.iter().map(|s| s.paid_cents).sum();
    let pending = sources.iter().map(|s| s.pending_cents).sum();
    (sources, paid, pending)
}

/// GET /api/v1/payouts/statement?payee=&from=&to= (`payouts.read`). `from` and `to` are
/// YYYY-MM-DD, inclusive.
pub async fn statement(pool: &SqlitePool, principal: &Principal, payee_id: &str, from: &str, to: &str) -> Result<Statement, PayoutError> {
    principal.require(scopes::PAYOUTS_READ)?;
    let parse = |s: &str, f: &str| NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").map_err(|_| format!("{f} must be a date (YYYY-MM-DD)"));
    let (from_d, to_d) = match (parse(from, "from"), parse(to, "to")) {
        (Ok(a), Ok(b)) => (a, b),
        (a, b) => return Err(PayoutError::Invalid([a.err(), b.err()].into_iter().flatten().collect())),
    };
    if to_d < from_d {
        return Err(PayoutError::Invalid(vec!["to must be on or after from".into()]));
    }
    if (to_d - from_d).num_days() > MAX_RANGE_DAYS {
        return Err(PayoutError::Invalid(vec![format!("the range is at most {MAX_RANGE_DAYS} days")]));
    }
    let payee = payees::load(pool, payee_id).await?.ok_or(PayoutError::NotFound("payee"))?;
    if payee.row.org_id != principal.org_id {
        return Err(PayoutError::NotFound("payee"));
    }
    let start = format!("{} 00:00:00", from_d.format("%Y-%m-%d"));
    let end = format!("{} 00:00:00", (to_d + chrono::Duration::days(1)).format("%Y-%m-%d"));
    let rows = sqlx::query_as::<_, LedgerEntry>(
        "SELECT * FROM payout_ledger
         WHERE (payee_id = ? OR (source_kind = 'merit' AND lower(github_username) IN (SELECT payee_key FROM payee_keys WHERE payee_id = ?)))
           AND datetime(created_at) >= datetime(?) AND datetime(created_at) < datetime(?)
         ORDER BY created_at, id",
    )
    .bind(payee_id)
    .bind(payee_id)
    .bind(&start)
    .bind(&end)
    .fetch_all(pool)
    .await?;
    let currency = rows.first().map(|r| r.currency.clone());
    let (sources, paid, pending) = summarize(rows);
    Ok(Statement {
        payee_id: payee.row.id.clone(),
        display_name: payee.row.display_name.clone(),
        from: from_d.format("%Y-%m-%d").to_string(),
        to: to_d.format("%Y-%m-%d").to_string(),
        currency,
        sources,
        paid_cents: paid,
        pending_cents: pending,
    })
}
