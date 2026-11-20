//! Time entries (contract 14.4 and 14.5): clocking in, the optional early clock-out, manager
//! adjust and void, and approval.
//!
//! A person clocks in; they do not clock out. The entry starts at the clock-in (never before
//! the shift start unless the shift pays from clock-in), whole minutes, and ends at the shift's
//! end, which is known and fixed the moment they clock in. One entry per shift occurrence
//! (`UNIQUE (occurrence_key)`): a second clock-in returns the same entry. A clock-in outside
//! every window is refused unless the payee has an open-shift allowance. Leaving early is the
//! exception: clock-out only ever shortens the open entry.
//!
//! States: `open` until the end time passes, then `closed` (derived from a passed-in clock),
//! `approved` by someone with `time.approve` who is not the payee (or by the scheduler when the
//! policy sets `time_auto_approve`), or `void`. A manager with `time.manage` may adjust or void
//! an entry with a reason; the payee never can. Once an approved entry has been paid into a
//! timesheet row it cannot be adjusted or voided: that would take back money already owed.
//!
//! `time.clock` acts only on the token member's own payee ([`own_payee`]); no body names a
//! payee. Every change is audited in the same transaction.

use super::error::PayoutError;
use super::payees::{self, Payee};
use super::shifts::{self, floor_minute, ClockInPlan};
use crate::audit::{ActorKind, AuditEntry};
use crate::auth::{member_payee_keys, scopes, tokens::format_db_time, tokens::parse_db_time, Directory, Principal};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

pub const OPEN: &str = "open";
pub const CLOSED: &str = "closed";
pub const APPROVED: &str = "approved";
pub const VOID: &str = "void";
/// Longest entry a manager may set (a 24 hour shift plus a daylight saving hour and margin).
pub const MAX_ENTRY_MINUTES: i64 = 26 * 60;
pub const AUTO_APPROVER: &str = "system:time-auto-approve";

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct EntryRow {
    pub id: String,
    pub org_id: String,
    pub payee_id: String,
    pub shift_id: String,
    pub occurrence_key: String,
    pub time_zone: String,
    pub clock_in_at: String,
    pub shift_start_at: String,
    pub shift_end_at: String,
    pub start_at: String,
    pub end_at: String,
    pub status: String,
    pub clocked_out_at: Option<String>,
    pub approved_by: Option<String>,
    pub approved_at: Option<String>,
    pub void_reason: Option<String>,
    pub clocked_in_by: String,
    pub created_at: String,
    pub updated_at: String,
}

impl EntryRow {
    pub fn start(&self) -> Option<DateTime<Utc>> {
        parse_db_time(&self.start_at)
    }
    pub fn end(&self) -> Option<DateTime<Utc>> {
        parse_db_time(&self.end_at)
    }
    pub fn minutes(&self) -> i64 {
        match (self.start(), self.end()) {
            (Some(s), Some(e)) if e > s => (e - s).num_seconds() / 60,
            _ => 0,
        }
    }
}

/// Pure: the state of an entry at `now`. A stored `open` entry whose end has passed is `closed`.
pub fn entry_state(status: &str, end: DateTime<Utc>, now: DateTime<Utc>) -> &'static str {
    match status {
        APPROVED => APPROVED,
        VOID => VOID,
        _ if end <= now => CLOSED,
        _ => OPEN,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EntryView {
    #[serde(flatten)]
    pub row: EntryRow,
    /// open | closed | approved | void
    pub state: &'static str,
    /// Whole minutes from start_at to end_at.
    pub minutes: i64,
}

pub fn view(row: EntryRow, now: DateTime<Utc>) -> EntryView {
    let state = row.end().map(|e| entry_state(&row.status, e, now)).unwrap_or(OPEN);
    let minutes = row.minutes();
    EntryView { row, state, minutes }
}

/// Who acted on an entry: a token member, or a chat account (a verified chat clock-in).
#[derive(Debug, Clone)]
pub struct Actor {
    pub kind: ActorKind,
    pub id: String,
    pub token_id: Option<String>,
}

impl Actor {
    pub fn of(p: &Principal) -> Actor {
        Actor { kind: crate::audit::kind_for_member(&p.member_id), id: p.member_id.clone(), token_id: Some(p.token_id.clone()) }
    }
    fn entry(&self, action: &str, target: String, mut detail: serde_json::Value) -> AuditEntry {
        if let (Some(t), serde_json::Value::Object(m)) = (&self.token_id, &mut detail) {
            m.insert("token_id".into(), json!(t));
        }
        AuditEntry::new(self.kind, self.id.clone(), action, target, detail)
    }
}

async fn load(conn: &mut SqliteConnection, id: &str) -> Result<Option<EntryRow>, sqlx::Error> {
    sqlx::query_as::<_, EntryRow>("SELECT * FROM time_entries WHERE id = ?").bind(id).fetch_optional(&mut *conn).await
}

async fn load_pool(pool: &SqlitePool, id: &str) -> Result<Option<EntryRow>, sqlx::Error> {
    let mut c = pool.acquire().await?;
    load(&mut c, id).await
}

#[allow(clippy::too_many_arguments)]
async fn event(conn: &mut SqliteConnection, entry: &EntryRow, action: &str, actor: &Actor, reason: Option<&str>, detail: serde_json::Value, now: &str) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO time_entry_events (entry_id, action, actor, reason, detail, created_at) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(&entry.id)
        .bind(action)
        .bind(&actor.id)
        .bind(reason)
        .bind(detail.to_string())
        .bind(now)
        .execute(&mut *conn)
        .await?;
    let mut d = json!({
        "entry_id": entry.id,
        "payee_id": entry.payee_id,
        "shift_id": entry.shift_id,
        "occurrence_key": entry.occurrence_key,
        "start_at": entry.start_at,
        "end_at": entry.end_at,
        "status": entry.status,
        "reason": reason,
    });
    if let (serde_json::Value::Object(m), serde_json::Value::Object(extra)) = (&mut d, detail) {
        for (k, v) in extra {
            m.insert(k, v);
        }
    }
    crate::audit::append_in(conn, &actor.entry(&format!("time.{action}"), format!("time_entry:{}", entry.id), d)).await?;
    Ok(())
}

// ------------------------------------------------------------------ scopes

/// `principal.require(scope)` for the time and wage functions. The mutation `no_time_scopes`
/// turns it off in test builds so the scope tests can be shown to fail.
pub fn need(principal: &Principal, scope: &str) -> Result<(), crate::auth::AuthError> {
    if super::mutant::on("no_time_scopes") {
        return Ok(());
    }
    principal.require(scope)
}

pub fn has(principal: &Principal, scope: &str) -> bool {
    super::mutant::on("no_time_scopes") || principal.has(scope)
}

// ------------------------------------------------------------------ who is the caller

/// The payee a `time.clock` token acts for: the active payee whose CRA member id is the
/// token's member, or whose payee keys the member declared. Exactly one, or refused.
pub async fn own_payee(pool: &SqlitePool, principal: &Principal) -> Result<Payee, PayoutError> {
    if super::mutant::on("own_payee_by_shift") {
        // The bug this guards: picking a payee by "has a shift now" instead of by who asked.
        let id: Option<String> = sqlx::query_scalar("SELECT payee_id FROM shifts WHERE org_id = ? AND status = 'active' ORDER BY created_at LIMIT 1")
            .bind(&principal.org_id)
            .fetch_optional(pool)
            .await?;
        if let Some(p) = match id { Some(id) => payees::load(pool, &id).await?, None => None } {
            return Ok(p);
        }
    }
    let mut ids: Vec<String> = sqlx::query_scalar("SELECT id FROM payees WHERE member_id = ? AND org_id = ? AND status <> 'closed' ORDER BY id")
        .bind(&principal.member_id)
        .bind(&principal.org_id)
        .fetch_all(pool)
        .await?;
    let mut keys: Vec<String> = principal.payee_keys.iter().map(|k| k.to_lowercase()).collect();
    keys.extend(member_payee_keys(pool, &principal.member_id).await?);
    for k in keys {
        let id: Option<String> = sqlx::query_scalar("SELECT payee_id FROM payee_keys WHERE payee_key = ?").bind(&k).fetch_optional(pool).await?;
        if let Some(id) = id {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    let mut found = Vec::new();
    for id in ids {
        if let Some(p) = payees::load(pool, &id).await? {
            if p.row.org_id == principal.org_id && p.row.status != "closed" {
                found.push(p);
            }
        }
    }
    match found.len() {
        0 => Err(PayoutError::Forbidden("this token's member is not a payee, so there is no time of theirs to clock".into())),
        1 => Ok(found.remove(0)),
        _ => Err(PayoutError::InvalidState("this token's member matches more than one payee; an administrator must fix the payee records".into())),
    }
}

// ------------------------------------------------------------------ clock in

#[derive(Debug, Clone, Serialize)]
pub struct ClockInResult {
    /// created | existing (a second clock-in for the same shift returns the same entry)
    pub result: &'static str,
    pub entry: EntryView,
}

async fn entry_containing(pool: &SqlitePool, payee_id: &str, now: &str) -> Result<Option<EntryRow>, sqlx::Error> {
    sqlx::query_as::<_, EntryRow>(
        "SELECT * FROM time_entries WHERE payee_id = ? AND status <> 'void' AND clock_in_at <= ? AND end_at > ? ORDER BY start_at LIMIT 1",
    )
    .bind(payee_id)
    .bind(now)
    .bind(now)
    .fetch_optional(pool)
    .await
}

async fn by_key(pool: &SqlitePool, key: &str) -> Result<Option<EntryRow>, sqlx::Error> {
    sqlx::query_as::<_, EntryRow>("SELECT * FROM time_entries WHERE occurrence_key = ?").bind(key).fetch_optional(pool).await
}

/// Clock in for `payee` at `now`. The caller has already established that `actor` may clock
/// in as this payee (REST: [`own_payee`]; chat: a verified identity).
pub async fn clock_in_for(pool: &SqlitePool, payee: &Payee, actor: &Actor, now: DateTime<Utc>) -> Result<ClockInResult, PayoutError> {
    if !payee.is_active() {
        return Err(PayoutError::InvalidState(format!("the payee is {}, so time cannot be clocked", payee.row.status)));
    }
    let now_s = format_db_time(now);
    // Already clocked in for whatever is running now: the same entry.
    if !super::mutant::on("second_clock_in_new_entry") {
        if let Some(e) = entry_containing(pool, &payee.row.id, &now_s).await? {
            return Ok(ClockInResult { result: "existing", entry: view(e, now) });
        }
    }
    let specs = shifts::active_specs(pool, &payee.row.id).await?;
    let plan: ClockInPlan = shifts::plan_clock_in(&specs, now).map_err(|r| PayoutError::InvalidState(r.message()))?;
    if let Some(e) = by_key(pool, &plan.occurrence_key).await? {
        if !super::mutant::on("second_clock_in_new_entry") {
            return Ok(ClockInResult { result: "existing", entry: view(e, now) });
        }
    }
    let mut start = plan.start;
    let end = plan.end;
    // Never overlap another entry of the same payee: start after any entry still running.
    let overlapping: Vec<EntryRow> = sqlx::query_as::<_, EntryRow>(
        "SELECT * FROM time_entries WHERE payee_id = ? AND status <> 'void' AND start_at < ? AND end_at > ?",
    )
    .bind(&payee.row.id)
    .bind(format_db_time(end))
    .bind(format_db_time(start))
    .fetch_all(pool)
    .await?;
    for o in &overlapping {
        if let Some(e) = o.end() {
            if e > start {
                start = e;
            }
        }
    }
    if start >= end {
        return Err(PayoutError::InvalidState("this shift is already covered by another time entry".into()));
    }
    let tz = shifts::parse_tz(
        &sqlx::query_scalar::<_, String>("SELECT time_zone FROM shifts WHERE id = ?")
            .bind(&plan.shift_id)
            .fetch_optional(pool)
            .await?
            .unwrap_or_else(|| "UTC".into()),
    )
    .map_err(|e| PayoutError::InvalidState(format!("stored shift time zone: {e}")))?;
    let row = EntryRow {
        id: Uuid::new_v4().to_string(),
        org_id: payee.row.org_id.clone(),
        payee_id: payee.row.id.clone(),
        shift_id: plan.shift_id.clone(),
        occurrence_key: if super::mutant::on("second_clock_in_new_entry") { format!("{}#{}", plan.occurrence_key, Uuid::new_v4()) } else { plan.occurrence_key.clone() },
        time_zone: tz.name().to_string(),
        clock_in_at: now_s.clone(),
        shift_start_at: format_db_time(plan.shift_start),
        shift_end_at: format_db_time(plan.shift_end),
        start_at: format_db_time(start),
        end_at: format_db_time(end),
        status: OPEN.into(),
        clocked_out_at: None,
        approved_by: None,
        approved_at: None,
        void_reason: None,
        clocked_in_by: actor.id.clone(),
        created_at: now_s.clone(),
        updated_at: now_s.clone(),
    };
    let mut tx = pool.begin().await?;
    // One statement: the overlap check and the insert cannot be raced.
    let res = sqlx::query(
        "INSERT INTO time_entries (id, org_id, payee_id, shift_id, occurrence_key, time_zone, clock_in_at, shift_start_at, shift_end_at,
         start_at, end_at, status, clocked_in_by, created_at, updated_at)
         SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
         WHERE NOT EXISTS (SELECT 1 FROM time_entries t WHERE t.payee_id = ? AND t.status <> 'void' AND t.start_at < ? AND t.end_at > ?)
         ON CONFLICT (occurrence_key) DO NOTHING",
    )
    .bind(&row.id)
    .bind(&row.org_id)
    .bind(&row.payee_id)
    .bind(&row.shift_id)
    .bind(&row.occurrence_key)
    .bind(&row.time_zone)
    .bind(&row.clock_in_at)
    .bind(&row.shift_start_at)
    .bind(&row.shift_end_at)
    .bind(&row.start_at)
    .bind(&row.end_at)
    .bind(&row.status)
    .bind(&row.clocked_in_by)
    .bind(&row.created_at)
    .bind(&row.updated_at)
    .bind(&row.payee_id)
    .bind(&row.end_at)
    .bind(&row.start_at)
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() == 0 {
        tx.rollback().await?;
        if let Some(e) = by_key(pool, &plan.occurrence_key).await? {
            return Ok(ClockInResult { result: "existing", entry: view(e, now) });
        }
        return Err(PayoutError::InvalidState("another time entry was clocked at the same moment; try again".into()));
    }
    event(&mut tx, &row, "clock_in", actor, None, json!({ "open_shift": plan.open_shift, "clock_in_at": row.clock_in_at }), &now_s).await?;
    tx.commit().await?;
    Ok(ClockInResult { result: "created", entry: view(row, now) })
}

/// POST /api/v1/time/clock-in (`time.clock`): the token member's own payee, never another.
pub async fn clock_in(pool: &SqlitePool, principal: &Principal, now: DateTime<Utc>) -> Result<ClockInResult, PayoutError> {
    need(principal, scopes::TIME_CLOCK)?;
    let payee = own_payee(pool, principal).await?;
    clock_in_for(pool, &payee, &Actor::of(principal), now).await
}

#[derive(Debug, Clone, Serialize)]
pub struct ClockOutResult {
    /// shortened | unchanged (a clock-out never extends an entry)
    pub result: &'static str,
    pub entry: EntryView,
}

/// POST /api/v1/time/clock-out (`time.clock`): leaving early. Shortens the caller's running
/// entry to now (whole minutes, rounded down). Never extends it.
pub async fn clock_out(pool: &SqlitePool, principal: &Principal, now: DateTime<Utc>) -> Result<ClockOutResult, PayoutError> {
    need(principal, scopes::TIME_CLOCK)?;
    let payee = own_payee(pool, principal).await?;
    let now_s = format_db_time(now);
    let row = sqlx::query_as::<_, EntryRow>(
        "SELECT * FROM time_entries WHERE payee_id = ? AND status = 'open' AND clock_in_at <= ? AND end_at > ? ORDER BY start_at LIMIT 1",
    )
    .bind(&payee.row.id)
    .bind(&now_s)
    .bind(&now_s)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| PayoutError::InvalidState("there is no running time entry to clock out of".into()))?;
    let (start, end) = match (row.start(), row.end()) {
        (Some(s), Some(e)) => (s, e),
        _ => return Err(PayoutError::InvalidState("stored entry times are unreadable".into())),
    };
    let new_end = if super::mutant::on("clock_out_extends") { floor_minute(now) + Duration::hours(2) } else { floor_minute(now).max(start).min(end) };
    if new_end == end {
        return Ok(ClockOutResult { result: "unchanged", entry: view(row, now) });
    }
    let mut tx = pool.begin().await?;
    let res = sqlx::query("UPDATE time_entries SET end_at = ?, clocked_out_at = ?, updated_at = ? WHERE id = ? AND end_at = ? AND status = 'open'")
        .bind(format_db_time(new_end))
        .bind(&now_s)
        .bind(&now_s)
        .bind(&row.id)
        .bind(&row.end_at)
        .execute(&mut *tx)
        .await?;
    if res.rows_affected() == 0 {
        return Err(PayoutError::InvalidState("the entry changed concurrently; read it again".into()));
    }
    let updated = load(&mut tx, &row.id).await?.ok_or(PayoutError::NotFound("time entry"))?;
    event(&mut tx, &updated, "clock_out", &Actor::of(principal), None, json!({ "end_before": row.end_at, "end_after": updated.end_at }), &now_s).await?;
    tx.commit().await?;
    Ok(ClockOutResult { result: "shortened", entry: view(updated, now) })
}

// ------------------------------------------------------------------ manager actions

async fn scoped(pool: &SqlitePool, principal: &Principal, id: &str) -> Result<EntryRow, PayoutError> {
    let row = load_pool(pool, id).await?.ok_or(PayoutError::NotFound("time entry"))?;
    if row.org_id != principal.org_id {
        return Err(PayoutError::NotFound("time entry"));
    }
    Ok(row)
}

/// Has this approved entry been paid into a timesheet row? A row written at or after the
/// entry's approval, for a period the entry overlaps, included it.
pub async fn is_settled(pool: &SqlitePool, row: &EntryRow) -> Result<bool, sqlx::Error> {
    if row.status != APPROVED {
        return Ok(false);
    }
    let Some(approved_at) = &row.approved_at else { return Ok(false) };
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM timesheet_rows WHERE payee_id = ? AND period_start < ? AND period_end > ? AND created_at >= ?",
    )
    .bind(&row.payee_id)
    .bind(&row.end_at)
    .bind(&row.start_at)
    .bind(approved_at)
    .fetch_one(pool)
    .await?;
    Ok(n > 0)
}

async fn refuse_payee(pool: &SqlitePool, directory: &dyn Directory, principal: &Principal, row: &EntryRow, verb: &str) -> Result<(), PayoutError> {
    if payees::principal_is_payee_of(pool, directory, principal, &row.payee_id).await? && !super::mutant::on(&format!("payee_{verb}s_own_entry")) {
        return Err(PayoutError::Forbidden(format!("a payee cannot {verb} their own time entry")));
    }
    Ok(())
}

fn reason_required(reason: Option<&str>) -> Result<String, PayoutError> {
    match reason.map(str::trim).filter(|r| !r.is_empty()) {
        Some(r) if r.chars().count() <= 500 => Ok(r.to_string()),
        Some(_) => Err(PayoutError::Invalid(vec!["reason must be at most 500 characters".into()])),
        None => Err(PayoutError::Invalid(vec!["a reason is required".into()])),
    }
}

/// Parse a whole-minute time: RFC 3339 (`2026-11-20T17:00:00Z`) or `YYYY-MM-DD HH:MM[:SS]` UTC.
pub fn parse_minute(s: &str, field: &str) -> Result<DateTime<Utc>, String> {
    let s = s.trim();
    let t = DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .ok()
        .or_else(|| parse_db_time(s))
        .or_else(|| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").ok().map(|n| n.and_utc()))
        .ok_or_else(|| format!("{field} must be a time such as 2026-11-20T17:00:00Z"))?;
    if floor_minute(t) != t {
        return Err(format!("{field} must be a whole minute"));
    }
    Ok(t)
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdjustBody {
    #[serde(default)]
    pub start_at: Option<String>,
    #[serde(default)]
    pub end_at: Option<String>,
    pub reason: Option<String>,
}

/// POST /api/v1/time/entries/{id}/adjust (`time.manage`): set a new start and/or end, with a
/// reason. Never the payee. An approved entry goes back to needing approval. Refused for a
/// void entry and for an entry already paid into a timesheet row.
pub async fn adjust(
    pool: &SqlitePool,
    directory: &dyn Directory,
    principal: &Principal,
    id: &str,
    body: AdjustBody,
    now: DateTime<Utc>,
) -> Result<EntryView, PayoutError> {
    need(principal, scopes::TIME_MANAGE)?;
    let row = scoped(pool, principal, id).await?;
    refuse_payee(pool, directory, principal, &row, "adjust").await?;
    let reason = reason_required(body.reason.as_deref())?;
    if row.status == VOID {
        return Err(PayoutError::InvalidState("a void entry cannot be adjusted".into()));
    }
    if is_settled(pool, &row).await? {
        return Err(PayoutError::InvalidState("this entry has been paid into a timesheet row and cannot be changed".into()));
    }
    let mut errs = Vec::new();
    let start = match &body.start_at {
        Some(s) => parse_minute(s, "start_at").map_err(|e| errs.push(e)).ok(),
        None => row.start(),
    };
    let end = match &body.end_at {
        Some(s) => parse_minute(s, "end_at").map_err(|e| errs.push(e)).ok(),
        None => row.end(),
    };
    if body.start_at.is_none() && body.end_at.is_none() {
        errs.push("give start_at, end_at or both".into());
    }
    let (Some(start), Some(end)) = (start, end) else { return Err(PayoutError::Invalid(errs)) };
    if end < start {
        errs.push("end_at must be on or after start_at".into());
    }
    if (end - start).num_minutes() > MAX_ENTRY_MINUTES {
        errs.push(format!("an entry is at most {MAX_ENTRY_MINUTES} minutes"));
    }
    if !errs.is_empty() {
        return Err(PayoutError::Invalid(errs));
    }
    let (start_s, end_s) = (format_db_time(start), format_db_time(end));
    let now_s = format_db_time(now);
    let mut tx = pool.begin().await?;
    let res = sqlx::query(
        "UPDATE time_entries SET start_at = ?, end_at = ?, status = CASE WHEN status = 'approved' THEN 'open' ELSE status END,
         approved_by = NULL, approved_at = NULL, updated_at = ?
         WHERE id = ? AND updated_at = ? AND status <> 'void'
           AND NOT EXISTS (SELECT 1 FROM time_entries t WHERE t.payee_id = time_entries.payee_id AND t.id <> time_entries.id
                           AND t.status <> 'void' AND t.start_at < ? AND t.end_at > ?)",
    )
    .bind(&start_s)
    .bind(&end_s)
    .bind(&now_s)
    .bind(id)
    .bind(&row.updated_at)
    .bind(&end_s)
    .bind(&start_s)
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() == 0 {
        return Err(PayoutError::InvalidState("the new times overlap another entry of this payee, or the entry changed concurrently".into()));
    }
    let updated = load(&mut tx, id).await?.ok_or(PayoutError::NotFound("time entry"))?;
    event(
        &mut tx,
        &updated,
        "adjust",
        &Actor::of(principal),
        Some(&reason),
        json!({ "start_before": row.start_at, "end_before": row.end_at, "status_before": row.status }),
        &now_s,
    )
    .await?;
    tx.commit().await?;
    Ok(view(updated, now))
}

/// POST /api/v1/time/entries/{id}/void (`time.manage`), with a reason. Never the payee. A void
/// entry pays nothing. Refused for an entry already paid into a timesheet row.
pub async fn void(
    pool: &SqlitePool,
    directory: &dyn Directory,
    principal: &Principal,
    id: &str,
    reason: Option<&str>,
    now: DateTime<Utc>,
) -> Result<EntryView, PayoutError> {
    need(principal, scopes::TIME_MANAGE)?;
    let row = scoped(pool, principal, id).await?;
    refuse_payee(pool, directory, principal, &row, "void").await?;
    let reason = reason_required(reason)?;
    if row.status == VOID {
        return Err(PayoutError::InvalidState("the entry is already void".into()));
    }
    if is_settled(pool, &row).await? {
        return Err(PayoutError::InvalidState("this entry has been paid into a timesheet row and cannot be voided".into()));
    }
    let now_s = format_db_time(now);
    let mut tx = pool.begin().await?;
    let res = sqlx::query("UPDATE time_entries SET status = 'void', void_reason = ?, updated_at = ? WHERE id = ? AND updated_at = ? AND status <> 'void'")
        .bind(&reason)
        .bind(&now_s)
        .bind(id)
        .bind(&row.updated_at)
        .execute(&mut *tx)
        .await?;
    if res.rows_affected() == 0 {
        return Err(PayoutError::InvalidState("the entry changed concurrently; read it again".into()));
    }
    let updated = load(&mut tx, id).await?.ok_or(PayoutError::NotFound("time entry"))?;
    event(&mut tx, &updated, "void", &Actor::of(principal), Some(&reason), json!({ "status_before": row.status }), &now_s).await?;
    tx.commit().await?;
    Ok(view(updated, now))
}

/// POST /api/v1/time/entries/{id}/approve (`time.approve`). Never the payee. Only a closed
/// entry (its end has passed) can be approved.
pub async fn approve(
    pool: &SqlitePool,
    directory: &dyn Directory,
    principal: &Principal,
    id: &str,
    now: DateTime<Utc>,
) -> Result<EntryView, PayoutError> {
    need(principal, scopes::TIME_APPROVE)?;
    let row = scoped(pool, principal, id).await?;
    refuse_payee(pool, directory, principal, &row, "approve").await?;
    let end = row.end().ok_or_else(|| PayoutError::InvalidState("stored entry times are unreadable".into()))?;
    match entry_state(&row.status, end, now) {
        CLOSED => {}
        APPROVED => return Ok(view(row, now)),
        OPEN => return Err(PayoutError::InvalidState("the entry is still open; it can be approved once its end time has passed".into())),
        _ => return Err(PayoutError::InvalidState("a void entry cannot be approved".into())),
    }
    let now_s = format_db_time(now);
    let mut tx = pool.begin().await?;
    let res = sqlx::query("UPDATE time_entries SET status = 'approved', approved_by = ?, approved_at = ?, updated_at = ? WHERE id = ? AND status = 'open' AND end_at <= ?")
        .bind(&principal.member_id)
        .bind(&now_s)
        .bind(&now_s)
        .bind(id)
        .bind(&now_s)
        .execute(&mut *tx)
        .await?;
    if res.rows_affected() == 0 {
        return Err(PayoutError::InvalidState("the entry changed concurrently; read it again".into()));
    }
    let updated = load(&mut tx, id).await?.ok_or(PayoutError::NotFound("time entry"))?;
    event(&mut tx, &updated, "approve", &Actor::of(principal), None, json!({ "minutes": updated.minutes() }), &now_s).await?;
    tx.commit().await?;
    Ok(view(updated, now))
}

/// The scheduler's auto-approval (policy `time_auto_approve`): every closed entry becomes
/// approved by `system:time-auto-approve`, each audited in its own transaction.
pub async fn auto_approve_closed(pool: &SqlitePool, now: DateTime<Utc>) -> Result<Vec<String>, sqlx::Error> {
    let now_s = format_db_time(now);
    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM time_entries WHERE status = 'open' AND end_at <= ? ORDER BY end_at, id")
        .bind(&now_s)
        .fetch_all(pool)
        .await?;
    let actor = Actor { kind: ActorKind::System, id: AUTO_APPROVER.into(), token_id: None };
    let mut done = Vec::new();
    for id in ids {
        let mut tx = pool.begin().await?;
        let res = sqlx::query("UPDATE time_entries SET status = 'approved', approved_by = ?, approved_at = ?, updated_at = ? WHERE id = ? AND status = 'open' AND end_at <= ?")
            .bind(AUTO_APPROVER)
            .bind(&now_s)
            .bind(&now_s)
            .bind(&id)
            .bind(&now_s)
            .execute(&mut *tx)
            .await?;
        if res.rows_affected() == 0 {
            continue;
        }
        let Some(updated) = load(&mut tx, &id).await? else { continue };
        event(&mut tx, &updated, "auto_approve", &actor, None, json!({ "minutes": updated.minutes() }), &now_s).await?;
        tx.commit().await?;
        done.push(id);
    }
    Ok(done)
}

// ------------------------------------------------------------------ reads

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryFilter {
    #[serde(default)]
    pub payee: Option<String>,
    /// YYYY-MM-DD, inclusive (UTC days of the entry's start).
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
}

fn day(s: &Option<String>, field: &str) -> Result<Option<NaiveDate>, PayoutError> {
    match s.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
        None => Ok(None),
        Some(v) => NaiveDate::parse_from_str(v, "%Y-%m-%d").map(Some).map_err(|_| PayoutError::Invalid(vec![format!("{field} must be a date YYYY-MM-DD")])),
    }
}

/// The payee a read is about: with `time.read`, any payee (or all); with only `time.clock`,
/// the caller's own payee and no other.
pub async fn read_scope(pool: &SqlitePool, principal: &Principal, payee: Option<&str>) -> Result<Option<String>, PayoutError> {
    if has(principal, scopes::TIME_READ) {
        return Ok(payee.map(str::to_string));
    }
    if has(principal, scopes::TIME_CLOCK) {
        let own = own_payee(pool, principal).await?;
        if payee.map_or(false, |p| p != own.row.id) && !super::mutant::on("clock_reads_others") {
            return Err(PayoutError::Forbidden("with time.clock only, you can read only your own time".into()));
        }
        return Ok(Some(if super::mutant::on("clock_reads_others") { payee.map(str::to_string).unwrap_or(own.row.id) } else { own.row.id }));
    }
    need(principal, scopes::TIME_READ)?;
    Ok(None)
}

/// GET /api/v1/time/entries?payee=&from=&to= (`time.read`, or `time.clock` for your own).
pub async fn list(pool: &SqlitePool, principal: &Principal, f: &EntryFilter, now: DateTime<Utc>) -> Result<Vec<EntryView>, PayoutError> {
    let payee = read_scope(pool, principal, f.payee.as_deref()).await?;
    let from = day(&f.from, "from")?.map(|d| format!("{} 00:00:00", d.format("%Y-%m-%d")));
    let to = day(&f.to, "to")?.and_then(|d| d.succ_opt()).map(|d| format!("{} 00:00:00", d.format("%Y-%m-%d")));
    let rows = sqlx::query_as::<_, EntryRow>(
        "SELECT * FROM time_entries WHERE org_id = ? AND (? IS NULL OR payee_id = ?) AND (? IS NULL OR start_at >= ?) AND (? IS NULL OR start_at < ?)
         ORDER BY start_at, id LIMIT 1000",
    )
    .bind(&principal.org_id)
    .bind(&payee)
    .bind(&payee)
    .bind(&from)
    .bind(&from)
    .bind(&to)
    .bind(&to)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| view(r, now)).collect())
}
