//! Invoices (contract 11.4): a payee, or staff on their behalf, asks to be paid.
//!
//! States: submitted -> approved -> scheduled -> paid, or submitted -> rejected, or
//! submitted | approved | scheduled -> void. Every change (and every approval) is an
//! `invoice_events` row with who and when. The total must equal the lines plus tax. The number
//! is unique per payee. Approval follows the payout rule: distinct approvers at or above
//! `min_approver_rank`, never the payee and never the submitter. An approved invoice becomes
//! exactly one ledger row (unique index on `(source_kind, source_ref, payee_id)`), payable on its
//! due date; payout runs pay it. A paid invoice can never be paid again; a rejected or void one
//! is never paid.

use super::config::PaymentConfig;
use super::error::PayoutError;
use super::ledger::{self, SourceRow, PAID, SOURCE_INVOICE};
use super::payees;
use super::payorders::MAX_CENTS;
use super::policy::{self, PayoutPolicy};
use crate::auth::{scopes, tokens::format_db_time, Directory, Principal};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{SqliteConnection, SqlitePool};
use std::collections::HashSet;
use uuid::Uuid;

pub const SUBMITTED: &str = "submitted";
pub const APPROVED: &str = "approved";
pub const REJECTED: &str = "rejected";
pub const SCHEDULED: &str = "scheduled";
pub const INV_PAID: &str = "paid";
pub const VOID: &str = "void";

const MAX_LINES: usize = 200;
const MAX_QUANTITY: i64 = 1_000_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvoiceLine {
    pub description: String,
    /// Whole units (1 or more).
    pub quantity: i64,
    pub unit_amount_cents: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewInvoice {
    pub payee_id: String,
    pub number: String,
    pub issue_date: NaiveDate,
    pub due_date: NaiveDate,
    /// Defaults to the payout policy currency; must equal it.
    #[serde(default)]
    pub currency: Option<String>,
    pub lines: Vec<InvoiceLine>,
    #[serde(default)]
    pub tax_cents: i64,
    pub total_cents: i64,
    #[serde(default)]
    pub memo: Option<String>,
    /// A reference to an attachment kept elsewhere (a URL or a document id). Not fetched.
    #[serde(default)]
    pub attachment_ref: Option<String>,
}

/// Pure: subtotal of the lines, or why they are refused. Exact in i128.
pub fn lines_subtotal(lines: &[InvoiceLine]) -> Result<i64, String> {
    if lines.is_empty() {
        return Err("an invoice needs at least one line".into());
    }
    if lines.len() > MAX_LINES {
        return Err(format!("at most {MAX_LINES} lines"));
    }
    let mut sum: i128 = 0;
    for (i, l) in lines.iter().enumerate() {
        if l.description.trim().is_empty() || l.description.chars().count() > 500 {
            return Err(format!("line {}: description is required (at most 500 characters)", i + 1));
        }
        if !(1..=MAX_QUANTITY).contains(&l.quantity) {
            return Err(format!("line {}: quantity must be between 1 and {MAX_QUANTITY}", i + 1));
        }
        if !(0..=MAX_CENTS).contains(&l.unit_amount_cents) {
            return Err(format!("line {}: unit_amount_cents must be between 0 and {MAX_CENTS}", i + 1));
        }
        sum += l.quantity as i128 * l.unit_amount_cents as i128;
    }
    if sum > MAX_CENTS as i128 {
        return Err(format!("the lines add up to more than {MAX_CENTS} cents"));
    }
    Ok(sum as i64)
}

/// Pure: the total must equal the sum of the lines plus tax. Returns the subtotal.
pub fn check_total(lines: &[InvoiceLine], tax_cents: i64, total_cents: i64) -> Result<i64, String> {
    let subtotal = lines_subtotal(lines)?;
    if !(0..=MAX_CENTS).contains(&tax_cents) {
        return Err(format!("tax_cents must be between 0 and {MAX_CENTS}"));
    }
    let expected = subtotal + tax_cents;
    if total_cents != expected {
        return Err(format!("total_cents {total_cents} does not equal the lines ({subtotal}) plus tax ({tax_cents}) = {expected}"));
    }
    if total_cents <= 0 {
        return Err("total_cents must be above 0".into());
    }
    Ok(subtotal)
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct InvoiceRow {
    pub id: String,
    pub org_id: String,
    pub payee_id: String,
    pub number: String,
    pub issue_date: String,
    pub due_date: String,
    pub currency: String,
    pub lines: String,
    pub subtotal_cents: i64,
    pub tax_cents: i64,
    pub total_cents: i64,
    pub memo: Option<String>,
    pub attachment_ref: Option<String>,
    pub status: String,
    pub ledger_id: Option<String>,
    pub submitted_by: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct InvoiceEvent {
    pub action: String,
    pub from_status: Option<String>,
    pub to_status: String,
    pub actor: String,
    pub note: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct InvoiceApproval {
    pub approver_member_id: String,
    pub created_at: String,
    pub counts: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InvoiceView {
    pub id: String,
    pub payee_id: String,
    pub number: String,
    pub issue_date: String,
    pub due_date: String,
    pub currency: String,
    pub lines: Value,
    pub subtotal_cents: i64,
    pub tax_cents: i64,
    pub total_cents: i64,
    pub memo: Option<String>,
    pub attachment_ref: Option<String>,
    pub status: String,
    pub ledger_id: Option<String>,
    pub submitted_by: String,
    pub created_at: String,
    pub updated_at: String,
    pub approvals: Vec<InvoiceApproval>,
    pub approvals_counted: i64,
    pub approvals_required: i64,
    pub events: Vec<InvoiceEvent>,
}

async fn load_row(pool: &SqlitePool, id: &str) -> Result<Option<InvoiceRow>, sqlx::Error> {
    sqlx::query_as::<_, InvoiceRow>("SELECT * FROM invoices WHERE id = ?").bind(id).fetch_optional(pool).await
}

async fn load_scoped(pool: &SqlitePool, principal: &Principal, id: &str) -> Result<InvoiceRow, PayoutError> {
    let row = load_row(pool, id).await?.ok_or(PayoutError::NotFound("invoice"))?;
    if row.org_id != principal.org_id {
        return Err(PayoutError::NotFound("invoice"));
    }
    Ok(row)
}

async fn event(conn: &mut SqliteConnection, id: &str, action: &str, from: Option<&str>, to: &str, actor: &str, note: Option<&str>, now: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO invoice_events (invoice_id, action, from_status, to_status, actor, note, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(action)
    .bind(from)
    .bind(to)
    .bind(actor)
    .bind(note)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    // Every invoice transition is audited in the same transaction: no audit entry, no change.
    let verb = match action {
        "submit" => "submitted",
        "approval" => "approval",
        "approve" => "approved",
        "schedule" => "scheduled",
        "reject" => "rejected",
        "void" => "voided",
        "paid" => "paid",
        other => other,
    };
    crate::audit::append_in(
        conn,
        &crate::audit::AuditEntry::new(
            crate::audit::kind_for_member(actor),
            actor,
            format!("invoice.{verb}"),
            format!("invoice:{id}"),
            serde_json::json!({ "invoice_id": id, "from_status": from, "to_status": to, "note": note }),
        ),
    )
    .await?;
    Ok(())
}

/// Approvals of an invoice that satisfy the policy: distinct, not the payee, not the submitter,
/// rank >= min_approver_rank.
async fn invoice_approvals(
    pool: &SqlitePool,
    directory: &dyn Directory,
    policy: &PayoutPolicy,
    row: &InvoiceRow,
) -> Result<(Vec<InvoiceApproval>, i64), sqlx::Error> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT approver_member_id, created_at FROM invoice_approvals WHERE invoice_id = ? ORDER BY created_at, approver_member_id",
    )
    .bind(&row.id)
    .fetch_all(pool)
    .await?;
    let payee = payees::load(pool, &row.payee_id).await?;
    let min = policy.approval.min_approver_rank;
    let mut counted: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for (m, at) in rows {
        let is_payee = match &payee {
            Some(p) => {
                let probe = Principal { token_id: String::new(), org_id: row.org_id.clone(), member_id: m.clone(), scopes: vec![], payee_keys: vec![] };
                payees::principal_is(pool, directory, &probe, p.row.member_id.as_deref(), &p.payee_keys).await?
            }
            None => true,
        };
        let rank_ok = match directory.rank_of(&m) {
            Some(r) => r >= min,
            None => min <= 0,
        };
        let reason = if is_payee {
            Some("the payee never approves their own invoice".to_string())
        } else if m == row.submitted_by {
            Some("the submitter never approves the invoice they submitted".to_string())
        } else if !rank_ok {
            Some(format!("rank below min_approver_rank {min}"))
        } else {
            None
        };
        if reason.is_none() {
            counted.insert(m.clone());
        }
        out.push(InvoiceApproval { approver_member_id: m, created_at: at, counts: reason.is_none(), reason });
    }
    let n = counted.len() as i64;
    Ok((out, n))
}

async fn view(pool: &SqlitePool, cfg: &PaymentConfig, directory: &dyn Directory, row: InvoiceRow) -> Result<InvoiceView, PayoutError> {
    let eff = policy::load_effective(pool, &row.org_id, cfg).await?;
    let (approvals, counted) = invoice_approvals(pool, directory, &eff.policy, &row).await?;
    let events = sqlx::query_as::<_, InvoiceEvent>(
        "SELECT action, from_status, to_status, actor, note, created_at FROM invoice_events WHERE invoice_id = ? ORDER BY id",
    )
    .bind(&row.id)
    .fetch_all(pool)
    .await?;
    Ok(InvoiceView {
        lines: serde_json::from_str(&row.lines).unwrap_or(Value::Null),
        id: row.id,
        payee_id: row.payee_id,
        number: row.number,
        issue_date: row.issue_date,
        due_date: row.due_date,
        currency: row.currency,
        subtotal_cents: row.subtotal_cents,
        tax_cents: row.tax_cents,
        total_cents: row.total_cents,
        memo: row.memo,
        attachment_ref: row.attachment_ref,
        status: row.status,
        ledger_id: row.ledger_id,
        submitted_by: row.submitted_by,
        created_at: row.created_at,
        updated_at: row.updated_at,
        approvals,
        approvals_counted: counted,
        approvals_required: eff.policy.approval.approvals_required.max(1),
        events,
    })
}

/// Mark scheduled invoices whose ledger row has been paid as `paid` (actor: whoever executed the
/// payment). Called by the scheduler tick and before every read.
pub async fn sync_paid(pool: &SqlitePool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT i.id, l.decided_by FROM invoices i JOIN payout_ledger l ON l.id = i.ledger_id WHERE i.status = ? AND l.status = ?",
    )
    .bind(SCHEDULED)
    .bind(PAID)
    .fetch_all(pool)
    .await?;
    let now_s = format_db_time(now);
    let mut n = 0;
    for (id, by) in rows {
        let mut tx = pool.begin().await?;
        let res = sqlx::query("UPDATE invoices SET status = ?, updated_at = ? WHERE id = ? AND status = ?")
            .bind(INV_PAID)
            .bind(&now_s)
            .bind(&id)
            .bind(SCHEDULED)
            .execute(&mut *tx)
            .await?;
        if res.rows_affected() > 0 {
            event(&mut tx, &id, "paid", Some(SCHEDULED), INV_PAID, by.as_deref().unwrap_or("system:ledger"), None, &now_s).await?;
            n += 1;
        }
        tx.commit().await?;
    }
    Ok(n)
}

// ------------------------------------------------------------------ REST / MCP functions

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvoiceFilter {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub payee_id: Option<String>,
}

/// GET /api/v1/invoices (`invoices.read`).
pub async fn list(pool: &SqlitePool, cfg: &PaymentConfig, directory: &dyn Directory, principal: &Principal, f: &InvoiceFilter, now: DateTime<Utc>) -> Result<Vec<InvoiceView>, PayoutError> {
    principal.require(scopes::INVOICES_READ)?;
    sync_paid(pool, now).await?;
    let rows = sqlx::query_as::<_, InvoiceRow>(
        "SELECT * FROM invoices WHERE org_id = ? AND (? IS NULL OR status = ?) AND (? IS NULL OR payee_id = ?) ORDER BY created_at, id",
    )
    .bind(&principal.org_id)
    .bind(&f.status)
    .bind(&f.status)
    .bind(&f.payee_id)
    .bind(&f.payee_id)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::new();
    for r in rows {
        out.push(view(pool, cfg, directory, r).await?);
    }
    Ok(out)
}

/// GET /api/v1/invoices/{id} (`invoices.read`).
pub async fn get(pool: &SqlitePool, cfg: &PaymentConfig, directory: &dyn Directory, principal: &Principal, id: &str, now: DateTime<Utc>) -> Result<InvoiceView, PayoutError> {
    principal.require(scopes::INVOICES_READ)?;
    sync_paid(pool, now).await?;
    let row = load_scoped(pool, principal, id).await?;
    view(pool, cfg, directory, row).await
}

/// POST /api/v1/invoices (`invoices.submit`). The payee may submit their own invoice (the one
/// thing a payee may do for themselves); staff may submit on a payee's behalf. The submitter is
/// the token's member.
pub async fn submit(
    pool: &SqlitePool,
    cfg: &PaymentConfig,
    directory: &dyn Directory,
    principal: &Principal,
    input: NewInvoice,
    now: DateTime<Utc>,
) -> Result<InvoiceView, PayoutError> {
    principal.require(scopes::INVOICES_SUBMIT)?;
    let mut errs = Vec::new();
    let payee = payees::load(pool, &input.payee_id).await?;
    match &payee {
        None => errs.push("payee not found".to_string()),
        Some(p) if p.row.org_id != principal.org_id => errs.push("payee not found".to_string()),
        Some(p) if p.row.status == "closed" => errs.push("payee is closed".to_string()),
        Some(_) => {}
    }
    let number = input.number.trim().to_string();
    if number.is_empty() || number.chars().count() > 64 {
        errs.push("number is required (at most 64 characters)".into());
    }
    if input.due_date < input.issue_date {
        errs.push("due_date must be on or after issue_date".into());
    }
    let subtotal = match check_total(&input.lines, input.tax_cents, input.total_cents) {
        Ok(s) => s,
        Err(e) => {
            errs.push(e);
            0
        }
    };
    let eff = policy::load_effective(pool, &principal.org_id, cfg).await?;
    let currency = input.currency.as_deref().map(|c| c.trim().to_lowercase()).unwrap_or_else(|| eff.policy.currency.to_lowercase());
    if !currency.eq_ignore_ascii_case(&eff.policy.currency) {
        errs.push(format!("currency must be the payout policy currency {}", eff.policy.currency));
    }
    if !errs.is_empty() {
        return Err(PayoutError::Invalid(errs));
    }
    let dup: Option<i64> = sqlx::query_scalar("SELECT 1 FROM invoices WHERE payee_id = ? AND number = ?")
        .bind(&input.payee_id)
        .bind(&number)
        .fetch_optional(pool)
        .await?;
    if dup.is_some() {
        return Err(PayoutError::Invalid(vec![format!("invoice number '{number}' already exists for this payee")]));
    }
    let id = Uuid::new_v4().to_string();
    let now_s = format_db_time(now);
    let mut tx = pool.begin().await?;
    let res = sqlx::query(
        "INSERT INTO invoices (id, org_id, payee_id, number, issue_date, due_date, currency, lines, subtotal_cents, tax_cents,
         total_cents, memo, attachment_ref, status, submitted_by, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&principal.org_id)
    .bind(&input.payee_id)
    .bind(&number)
    .bind(input.issue_date.format("%Y-%m-%d").to_string())
    .bind(input.due_date.format("%Y-%m-%d").to_string())
    .bind(&currency)
    .bind(serde_json::to_string(&input.lines).expect("serializable"))
    .bind(subtotal)
    .bind(input.tax_cents)
    .bind(input.total_cents)
    .bind(input.memo.as_deref().map(str::trim).filter(|m| !m.is_empty()))
    .bind(input.attachment_ref.as_deref().map(str::trim).filter(|m| !m.is_empty()))
    .bind(SUBMITTED)
    .bind(&principal.member_id)
    .bind(&now_s)
    .bind(&now_s)
    .execute(&mut *tx)
    .await;
    match res {
        Ok(_) => {}
        Err(sqlx::Error::Database(d)) if d.message().contains("UNIQUE") => {
            return Err(PayoutError::Invalid(vec![format!("invoice number '{number}' already exists for this payee")]));
        }
        Err(e) => return Err(e.into()),
    }
    event(&mut tx, &id, "submit", None, SUBMITTED, &principal.member_id, None, &now_s).await?;
    tx.commit().await?;
    let row = load_scoped(pool, principal, &id).await?;
    view(pool, cfg, directory, row).await
}

async fn refuse_payee(pool: &SqlitePool, directory: &dyn Directory, principal: &Principal, row: &InvoiceRow, verb: &str) -> Result<(), PayoutError> {
    if payees::principal_is_payee_of(pool, directory, principal, &row.payee_id).await? {
        return Err(PayoutError::Forbidden(format!("a payee cannot {verb} their own invoice")));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct ApproveInvoiceResult {
    /// recorded | already_approved
    pub result: &'static str,
    pub approver_member_id: String,
    pub invoice: InvoiceView,
}

/// POST /api/v1/invoices/{id}/approve (`invoices.approve`). Records the token member's approval.
/// When the counted approvals reach the policy's approvals_required, the invoice is approved and
/// becomes exactly one ledger row (payable on its due date), in one transaction.
pub async fn approve(
    pool: &SqlitePool,
    cfg: &PaymentConfig,
    directory: &dyn Directory,
    principal: &Principal,
    id: &str,
    now: DateTime<Utc>,
) -> Result<ApproveInvoiceResult, PayoutError> {
    principal.require(scopes::INVOICES_APPROVE)?;
    let row = load_scoped(pool, principal, id).await?;
    refuse_payee(pool, directory, principal, &row, "approve").await?;
    if principal.member_id == row.submitted_by {
        return Err(PayoutError::Forbidden("the submitter of an invoice can never approve it".into()));
    }
    if row.status != SUBMITTED {
        return Err(PayoutError::InvalidState(format!("invoice is '{}' and cannot be approved", row.status)));
    }
    let now_s = format_db_time(now);
    let mut tx = pool.begin().await?;
    let res = sqlx::query(
        "INSERT OR IGNORE INTO invoice_approvals (invoice_id, approver_member_id, org_id, token_id, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(&principal.member_id)
    .bind(&principal.org_id)
    .bind(&principal.token_id)
    .bind(&now_s)
    .execute(&mut *tx)
    .await?;
    let recorded = res.rows_affected() > 0;
    if recorded {
        event(&mut tx, id, "approval", Some(SUBMITTED), SUBMITTED, &principal.member_id, None, &now_s).await?;
    }
    tx.commit().await?;

    let eff = policy::load_effective(pool, &row.org_id, cfg).await?;
    let (approvals, counted) = invoice_approvals(pool, directory, &eff.policy, &row).await?;
    if counted >= eff.policy.approval.approvals_required.max(1) {
        let payee = payees::load(pool, &row.payee_id).await?.ok_or(PayoutError::NotFound("payee"))?;
        let approvers: Vec<String> = approvals.into_iter().filter(|a| a.counts).map(|a| a.approver_member_id).collect();
        let due = NaiveDate::parse_from_str(&row.due_date, "%Y-%m-%d")
            .map_err(|_| PayoutError::InvalidState("stored due date is unreadable".into()))?;
        let pay_after = format!("{} 00:00:00", due.format("%Y-%m-%d"));
        let mut tx = pool.begin().await?;
        let moved = sqlx::query("UPDATE invoices SET status = ?, updated_at = ? WHERE id = ? AND status = ?")
            .bind(APPROVED)
            .bind(&now_s)
            .bind(id)
            .bind(SUBMITTED)
            .execute(&mut *tx)
            .await?;
        if moved.rows_affected() == 1 {
            event(&mut tx, id, "approve", Some(SUBMITTED), APPROVED, &principal.member_id, None, &now_s).await?;
            let contributor_id: Option<String> = sqlx::query_scalar(
                "SELECT CASE WHEN typeof(c.id) = 'blob' THEN lower(substr(hex(c.id),1,8)||'-'||substr(hex(c.id),9,4)||'-'||substr(hex(c.id),13,4)||'-'||substr(hex(c.id),17,4)||'-'||substr(hex(c.id),21)) ELSE c.id END
                 FROM contributors c JOIN payee_keys k ON k.payee_key = lower(c.github_username) WHERE k.payee_id = ? ORDER BY k.payee_key LIMIT 1",
            )
            .bind(&row.payee_id)
            .fetch_optional(&mut *tx)
            .await?;
            let src = SourceRow {
                source_kind: SOURCE_INVOICE,
                source_ref: row.id.clone(),
                payee_id: row.payee_id.clone(),
                payee_key: payee.ledger_key(),
                contributor_id,
                title: format!("Invoice {} ({})", row.number, payee.row.display_name),
                amount_cents: row.total_cents,
                currency: row.currency.clone(),
                pay_after: Some(pay_after),
                created_at: now_s.clone(),
            };
            let Some(lid) = ledger::insert_source_row(&mut tx, &src).await? else {
                tx.rollback().await?;
                return Err(PayoutError::InvalidState("this invoice already has a ledger row".into()));
            };
            for m in &approvers {
                sqlx::query("INSERT OR IGNORE INTO payout_approvals (ledger_id, approver_member_id, org_id, token_id, created_at) VALUES (?, ?, ?, ?, ?)")
                    .bind(&lid)
                    .bind(m)
                    .bind(&row.org_id)
                    .bind(format!("invoice:{}", row.id))
                    .bind(&now_s)
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query("UPDATE invoices SET status = ?, ledger_id = ?, updated_at = ? WHERE id = ? AND status = ?")
                .bind(SCHEDULED)
                .bind(&lid)
                .bind(&now_s)
                .bind(id)
                .bind(APPROVED)
                .execute(&mut *tx)
                .await?;
            event(&mut tx, id, "schedule", Some(APPROVED), SCHEDULED, &principal.member_id, Some(&format!("ledger row {lid}")), &now_s).await?;
        }
        tx.commit().await?;
    }
    let row = load_scoped(pool, principal, id).await?;
    Ok(ApproveInvoiceResult {
        result: if recorded { "recorded" } else { "already_approved" },
        approver_member_id: principal.member_id.clone(),
        invoice: view(pool, cfg, directory, row).await?,
    })
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReasonBody {
    #[serde(default)]
    pub reason: Option<String>,
}

/// POST /api/v1/invoices/{id}/reject (`invoices.approve`). Only a submitted invoice.
pub async fn reject(
    pool: &SqlitePool,
    cfg: &PaymentConfig,
    directory: &dyn Directory,
    principal: &Principal,
    id: &str,
    reason: Option<&str>,
    now: DateTime<Utc>,
) -> Result<InvoiceView, PayoutError> {
    principal.require(scopes::INVOICES_APPROVE)?;
    let row = load_scoped(pool, principal, id).await?;
    refuse_payee(pool, directory, principal, &row, "reject").await?;
    let now_s = format_db_time(now);
    let mut tx = pool.begin().await?;
    let res = sqlx::query("UPDATE invoices SET status = ?, updated_at = ? WHERE id = ? AND status = ?")
        .bind(REJECTED)
        .bind(&now_s)
        .bind(id)
        .bind(SUBMITTED)
        .execute(&mut *tx)
        .await?;
    if res.rows_affected() == 0 {
        return Err(PayoutError::InvalidState(format!("invoice is '{}' and cannot be rejected", row.status)));
    }
    event(&mut tx, id, "reject", Some(SUBMITTED), REJECTED, &principal.member_id, reason, &now_s).await?;
    tx.commit().await?;
    let row = load_scoped(pool, principal, id).await?;
    view(pool, cfg, directory, row).await
}

/// POST /api/v1/invoices/{id}/void (`invoices.approve`). A submitted, approved or scheduled
/// invoice. Its ledger row, if any, is rejected first (atomically, only while it is unpaid); a
/// row already being paid or paid makes the void fail.
pub async fn void(
    pool: &SqlitePool,
    cfg: &PaymentConfig,
    directory: &dyn Directory,
    principal: &Principal,
    id: &str,
    reason: Option<&str>,
    now: DateTime<Utc>,
) -> Result<InvoiceView, PayoutError> {
    principal.require(scopes::INVOICES_APPROVE)?;
    sync_paid(pool, now).await?;
    let row = load_scoped(pool, principal, id).await?;
    refuse_payee(pool, directory, principal, &row, "void").await?;
    if ![SUBMITTED, APPROVED, SCHEDULED].contains(&row.status.as_str()) {
        return Err(PayoutError::InvalidState(format!("invoice is '{}' and cannot be voided", row.status)));
    }
    if let Some(lid) = &row.ledger_id {
        if let Err(e) = ledger::reject(pool, lid, &principal.member_id, "invoice voided").await {
            return Err(match e {
                ledger::LedgerError::InvalidState(st, _) => PayoutError::InvalidState(format!("the invoice's ledger row is '{st}'; it cannot be voided")),
                other => other.into(),
            });
        }
    }
    let now_s = format_db_time(now);
    let mut tx = pool.begin().await?;
    let res = sqlx::query("UPDATE invoices SET status = ?, updated_at = ? WHERE id = ? AND status = ?")
        .bind(VOID)
        .bind(&now_s)
        .bind(id)
        .bind(&row.status)
        .execute(&mut *tx)
        .await?;
    if res.rows_affected() == 0 {
        return Err(PayoutError::InvalidState("invoice changed concurrently; read it again".into()));
    }
    event(&mut tx, id, "void", Some(&row.status), VOID, &principal.member_id, reason, &now_s).await?;
    tx.commit().await?;
    let row = load_scoped(pool, principal, id).await?;
    view(pool, cfg, directory, row).await
}
