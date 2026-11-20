//! From time to pay (contract 14.5).
//!
//! Layout:
//! - Pay periods come from the payout policy's schedule: weekly, every two weeks or monthly,
//!   the same slots as the payout runs ([`pay_period`]). A manual schedule uses calendar
//!   months (UTC).
//! - Period pay is pure ([`period_pay`]): approved minutes per day (split at local midnight
//!   and at the period's boundaries) times the rate in force that day, overtime beyond the
//!   weekly threshold if the rate sets one, summed exactly and floored to the cent once per
//!   period. The fraction of a cent left over is reported, never paid.
//! - [`materialize_due`], called from the scheduler tick, writes one ledger row (source
//!   `timesheet`) per payee per ended period, exactly once (`UNIQUE (payee_id, period_key,
//!   seq)` and the ledger's source index). Time approved after that row exists raises what
//!   the period owes; the difference becomes a correction row (`seq` 1, 2, ...), never a change
//!   to an existing row. The ledger rows then go through the unchanged approval, limit, hold,
//!   block, eligibility and kill switch path; nothing here transfers money.

use super::api::ApiContext;
use super::config::PaymentConfig;
use super::error::PayoutError;
use super::ledger::{self, SourceRow, SOURCE_TIMESHEET};
use super::payees;
use super::policy::{self, Schedule};
use super::schedule::{current_slot, PeriodSlot};
use super::shifts::{self, day_segments};
use super::timeclock::{self, EntryRow, EntryView};
use super::wages::{self, rate_on, WageRate};
use crate::auth::{tokens::format_db_time, tokens::parse_db_time, Directory, Principal};
use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use serde::Serialize;
use serde_json::json;
use sqlx::SqlitePool;
use std::collections::{BTreeMap, HashMap};
use tracing::{info, warn};
use uuid::Uuid;

/// Value units per cent: minutes x (cents per hour) x (basis points) / (60 x 10000).
pub const UNITS_PER_CENT: i128 = 60 * 10_000;
/// Base multiplier: 1x in basis points.
pub const BASE_BPS: i64 = 10_000;
/// Ended periods older than this are not recomputed (their rows are final).
pub const LOOKBACK_DAYS: i64 = 366;

// ================================================================== periods (pure)

fn calendar_month(t: DateTime<Utc>) -> PeriodSlot {
    let first = NaiveDate::from_ymd_opt(t.year(), t.month(), 1).expect("valid date");
    let (ny, nm) = if t.month() == 12 { (t.year() + 1, 1) } else { (t.year(), t.month() + 1) };
    let next = NaiveDate::from_ymd_opt(ny, nm, 1).expect("valid date");
    PeriodSlot {
        key: format!("calendar-{:04}-{:02}", t.year(), t.month()),
        start: Utc.from_utc_datetime(&first.and_hms_opt(0, 0, 0).expect("valid time")),
        end: Utc.from_utc_datetime(&next.and_hms_opt(0, 0, 0).expect("valid time")),
    }
}

/// Pure: the pay period containing `t`: the payout schedule's slot (weekly, biweekly or
/// monthly), or the calendar month (UTC) for a manual schedule.
pub fn pay_period(schedule: &Schedule, t: DateTime<Utc>) -> PeriodSlot {
    current_slot(schedule, t).filter(|s| s.start <= t && t < s.end).unwrap_or_else(|| calendar_month(t))
}

/// Pure: every pay period overlapping [start, end), in order.
pub fn periods_overlapping(schedule: &Schedule, start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<PeriodSlot> {
    let mut out = Vec::new();
    let mut p = pay_period(schedule, start);
    let mut guard = 0;
    while p.start < end && guard < 500 {
        guard += 1;
        let next = p.end;
        out.push(p);
        p = pay_period(schedule, next);
    }
    out
}

/// Pure: the period a key names (a key printed by a timesheet, such as `weekly-2026-11-20`,
/// `monthly-2026-11` or `calendar-2026-11`), or the period containing noon UTC of a date.
pub fn period_for(schedule: &Schedule, period: &str) -> Result<PeriodSlot, String> {
    let p = period.trim();
    let bad = || format!("period '{p}' is neither a period key (from a timesheet) nor a date YYYY-MM-DD");
    if let Ok(d) = NaiveDate::parse_from_str(p, "%Y-%m-%d") {
        return Ok(pay_period(schedule, Utc.from_utc_datetime(&d.and_hms_opt(12, 0, 0).expect("valid"))));
    }
    let (_, rest) = p.split_once('-').ok_or_else(bad)?;
    let probe = if let Ok(d) = NaiveDate::parse_from_str(rest, "%Y-%m-%d") {
        let hour = schedule.hour_utc.unwrap_or(0).clamp(0, 23) as u32;
        Utc.from_utc_datetime(&d.and_hms_opt(hour, 0, 0).expect("valid"))
    } else if let Ok(d) = NaiveDate::parse_from_str(&format!("{rest}-28"), "%Y-%m-%d") {
        Utc.from_utc_datetime(&d.and_hms_opt(12, 0, 0).expect("valid"))
    } else {
        return Err(bad());
    };
    let slot = pay_period(schedule, probe);
    if slot.key != p {
        return Err(format!("period '{p}' is not a period of the current pay schedule (the period there is '{}')", slot.key));
    }
    Ok(slot)
}

// ================================================================== period pay (pure)

/// Worked time of one approved entry inside one local day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkSegment {
    pub entry_id: String,
    pub date: NaiveDate,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub minutes: i64,
}

/// Pure: split an entry into per-day segments in its time zone.
pub fn entry_segments(entry_id: &str, start: DateTime<Utc>, end: DateTime<Utc>, tz: &chrono_tz::Tz) -> Vec<WorkSegment> {
    day_segments(start, end, tz)
        .into_iter()
        .map(|d| WorkSegment { entry_id: entry_id.to_string(), date: d.date, start: d.start, end: d.end, minutes: d.minutes })
        .collect()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DayLine {
    pub date: String,
    pub minutes: i64,
    pub regular_minutes: i64,
    pub overtime_minutes: i64,
    /// Minutes with no approved rate in force that day: not paid.
    pub unpaid_minutes: i64,
    pub rate_cents_per_hour: Option<i64>,
    pub overtime_bps: Option<i64>,
    pub wage_id: Option<String>,
    /// Exact value of the day in cents (before the period's floor), as text: `123.456789`.
    pub exact_cents: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PeriodPay {
    pub minutes: i64,
    pub regular_minutes: i64,
    pub overtime_minutes: i64,
    pub unpaid_minutes: i64,
    pub days: Vec<DayLine>,
    /// floor(exact value), the amount paid.
    pub cents: i64,
    /// The fraction of a cent not paid: leftover_units / units_per_cent.
    pub leftover_units: i64,
    pub units_per_cent: i64,
    pub notes: Vec<String>,
}

fn exact(units: i128) -> String {
    let whole = units / UNITS_PER_CENT;
    let frac = units % UNITS_PER_CENT;
    format!("{whole}.{:06}", frac * 1_000_000 / UNITS_PER_CENT)
}

/// Pure: what a period owes. `segments` are the payee's approved work, split per local day,
/// from at least the Monday before the period so weekly overtime can count earlier minutes of
/// the same week. Each piece is cut at the period's start and end; only pieces inside
/// [period_start, period_end) are paid. Minutes beyond the threshold of the rate in force
/// that day, counted in order through each week (Monday to Sunday by local date), are paid at
/// the overtime multiplier. The sum is exact and floored to the cent once.
pub fn period_pay(segments: &[WorkSegment], period_start: DateTime<Utc>, period_end: DateTime<Utc>, rates: &[WageRate]) -> PeriodPay {
    // Cut at the period boundaries.
    let mut pieces: Vec<WorkSegment> = Vec::new();
    for s in segments {
        let mut cuts = vec![s.start];
        for b in [period_start, period_end] {
            if s.start < b && b < s.end && !super::mutant::on("no_period_cut") {
                cuts.push(b);
            }
        }
        cuts.push(s.end);
        cuts.sort();
        for w in cuts.windows(2) {
            if w[1] > w[0] {
                pieces.push(WorkSegment { entry_id: s.entry_id.clone(), date: s.date, start: w[0], end: w[1], minutes: (w[1] - w[0]).num_seconds() / 60 });
            }
        }
    }
    pieces.sort_by(|a, b| (a.start, &a.entry_id).cmp(&(b.start, &b.entry_id)));
    let mut week_minutes: HashMap<(i32, u32), i64> = HashMap::new();
    let mut days: BTreeMap<NaiveDate, (DayLine, i128)> = BTreeMap::new();
    let mut pay = PeriodPay { units_per_cent: UNITS_PER_CENT as i64, ..Default::default() };
    let mut total: i128 = 0;
    let first_date = pieces.iter().find(|p| p.end > period_start).map(|p| p.date);
    for p in pieces {
        if p.start >= period_end && !super::mutant::on("no_period_cut") {
            continue;
        }
        let wk = p.date.iso_week();
        let before = *week_minutes.get(&(wk.year(), wk.week())).unwrap_or(&0);
        week_minutes.insert((wk.year(), wk.week()), before + p.minutes);
        if p.end <= period_start && !super::mutant::on("no_period_cut") {
            continue; // earlier in the week, before this period: counted for overtime only
        }
        let rate_day = if super::mutant::on("one_rate_per_period") { first_date.unwrap_or(p.date) } else { p.date };
        let rate = rate_on(rates, rate_day);
        let (regular, overtime) = match rate.and_then(|r| r.overtime_after_minutes.zip(r.overtime_bps)) {
            Some((after, _)) if !super::mutant::on("no_overtime") => {
                let regular = (after - before).clamp(0, p.minutes);
                (regular, p.minutes - regular)
            }
            _ => (p.minutes, 0),
        };
        let line = days.entry(p.date).or_insert_with(|| (DayLine { date: p.date.format("%Y-%m-%d").to_string(), ..Default::default() }, 0));
        line.0.minutes += p.minutes;
        pay.minutes += p.minutes;
        match rate {
            None => {
                line.0.unpaid_minutes += p.minutes;
                pay.unpaid_minutes += p.minutes;
            }
            Some(r) => {
                let ot_bps = r.overtime_bps.unwrap_or(BASE_BPS);
                let units = regular as i128 * r.rate_cents_per_hour as i128 * BASE_BPS as i128
                    + overtime as i128 * r.rate_cents_per_hour as i128 * ot_bps as i128;
                line.0.regular_minutes += regular;
                line.0.overtime_minutes += overtime;
                line.0.rate_cents_per_hour = Some(r.rate_cents_per_hour);
                line.0.overtime_bps = r.overtime_bps;
                line.0.wage_id = Some(r.id.clone());
                line.1 += units;
                pay.regular_minutes += regular;
                pay.overtime_minutes += overtime;
                total += units;
            }
        }
    }
    for (_, (mut line, units)) in days {
        line.exact_cents = exact(units);
        pay.days.push(line);
    }
    if super::mutant::on("round_up") {
        pay.cents = ((total + UNITS_PER_CENT - 1) / UNITS_PER_CENT) as i64;
    } else {
        pay.cents = (total / UNITS_PER_CENT) as i64;
    }
    pay.leftover_units = (total % UNITS_PER_CENT) as i64;
    if pay.unpaid_minutes > 0 {
        pay.notes.push(format!("{} minutes fell on days with no approved rate and are not paid", pay.unpaid_minutes));
    }
    pay
}

// ================================================================== loading

/// Approved work of one payee that ends after `from`, split per local day.
async fn approved_segments(pool: &SqlitePool, payee_id: &str, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<WorkSegment>, sqlx::Error> {
    let status = if super::mutant::on("void_pays") { "void" } else { timeclock::APPROVED };
    let rows = sqlx::query_as::<_, EntryRow>(
        "SELECT * FROM time_entries WHERE payee_id = ? AND (status = ? OR status = 'approved') AND end_at > ? AND start_at < ? ORDER BY start_at, id",
    )
    .bind(payee_id)
    .bind(status)
    .bind(format_db_time(from))
    .bind(format_db_time(to))
    .fetch_all(pool)
    .await?;
    let mut out = Vec::new();
    for r in rows {
        let (Some(s), Some(e)) = (r.start(), r.end()) else { continue };
        let Ok(tz) = shifts::parse_tz(&r.time_zone) else {
            warn!("time entry {} has an unreadable time zone; not paid", r.id);
            continue;
        };
        let segs = if super::mutant::on("no_midnight_split") {
            vec![WorkSegment { entry_id: r.id.clone(), date: shifts::local_date(&tz, s), start: s, end: e, minutes: (e - s).num_seconds() / 60 }]
        } else {
            entry_segments(&r.id, s, e, &tz)
        };
        out.extend(segs);
    }
    Ok(out)
}

/// What a period owes a payee right now (approved time only), with the rates used.
pub async fn compute(
    pool: &SqlitePool,
    directory: &dyn Directory,
    policy: &policy::PayoutPolicy,
    payee_id: &str,
    period: &PeriodSlot,
) -> Result<PeriodPay, sqlx::Error> {
    let rates = wages::approved_rates(pool, directory, policy, payee_id).await?;
    let segments = approved_segments(pool, payee_id, period.start - Duration::days(8), period.end).await?;
    Ok(period_pay(&segments, period.start, period.end, &rates))
}

// ================================================================== materialize

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TimesheetReport {
    /// (payee id, period key, seq, ledger row id or None for a zero row)
    pub written: Vec<(String, String, i64, Option<String>)>,
    pub auto_approved: Vec<String>,
    /// (payee id, period key, why nothing was written)
    pub held: Vec<(String, String, String)>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct TimesheetRow {
    pub id: String,
    pub payee_id: String,
    pub period_key: String,
    pub period_start: String,
    pub period_end: String,
    pub seq: i64,
    pub minutes: i64,
    pub owed_cents: i64,
    pub amount_cents: i64,
    pub ledger_id: Option<String>,
    pub detail: String,
    pub created_at: String,
}

/// The ledger source ref of a timesheet row: `<payee>:<period>` or `<payee>:<period>:<n>`.
pub fn source_ref(payee_id: &str, period_key: &str, seq: i64) -> String {
    if seq == 0 {
        format!("{payee_id}:{period_key}")
    } else {
        format!("{payee_id}:{period_key}:{seq}")
    }
}

/// Write what one ended period owes one payee: the first row once, then a correction row only
/// when the period now owes more than its rows already carry. Returns what was written.
async fn write_period(
    pool: &SqlitePool,
    payee: &payees::Payee,
    currency: &str,
    period: &PeriodSlot,
    pay: &PeriodPay,
    now: DateTime<Utc>,
) -> Result<Result<Option<(i64, Option<String>)>, String>, sqlx::Error> {
    let now_s = format_db_time(now);
    let (ps, pe) = (format_db_time(period.start), format_db_time(period.end));
    let mut tx = pool.begin().await?;
    // A period of another key overlapping this one means the pay schedule changed: never pay
    // the same minutes twice. Fail closed; an operator settles it.
    let clash: Option<String> = sqlx::query_scalar(
        "SELECT period_key FROM timesheet_rows WHERE payee_id = ? AND period_key <> ? AND period_start < ? AND period_end > ? LIMIT 1",
    )
    .bind(&payee.row.id)
    .bind(&period.key)
    .bind(&pe)
    .bind(&ps)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(k) = clash {
        tx.rollback().await?;
        return Ok(Err(format!("overlaps the already written period {k} (the pay schedule changed); settle it by hand")));
    }
    let rows: Vec<(i64, i64)> = sqlx::query_as("SELECT seq, amount_cents FROM timesheet_rows WHERE payee_id = ? AND period_key = ? ORDER BY seq")
        .bind(&payee.row.id)
        .bind(&period.key)
        .fetch_all(&mut *tx)
        .await?;
    let rows = if super::mutant::on("timesheet_twice") { Vec::new() } else { rows };
    let carried: i64 = rows.iter().map(|r| r.1).sum();
    let (seq, amount) = if rows.is_empty() {
        if pay.minutes == 0 && pay.cents == 0 {
            tx.rollback().await?;
            return Ok(Ok(None));
        }
        (0, pay.cents)
    } else {
        let diff = pay.cents - carried;
        if diff < 0 {
            tx.rollback().await?;
            return Ok(Err(format!("the period now owes {} cents, less than the {carried} already written; nothing is taken back automatically", pay.cents)));
        }
        if diff == 0 {
            tx.rollback().await?;
            return Ok(Ok(None));
        }
        let next = rows.iter().map(|r| r.0).max().unwrap_or(0) + 1;
        if super::mutant::on("correction_edits_first") {
            sqlx::query("UPDATE timesheet_rows SET amount_cents = amount_cents + ?, owed_cents = ? WHERE payee_id = ? AND period_key = ? AND seq = 0")
                .bind(diff)
                .bind(pay.cents)
                .bind(&payee.row.id)
                .bind(&period.key)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE payout_ledger SET amount_cents = amount_cents + ? WHERE source_kind = 'timesheet' AND source_ref = ?")
                .bind(diff)
                .bind(source_ref(&payee.row.id, &period.key, 0))
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(Ok(None));
        }
        (next, diff)
    };
    let id = Uuid::new_v4().to_string();
    let detail = json!({ "pay": pay });
    let res = sqlx::query(
        "INSERT OR IGNORE INTO timesheet_rows (id, org_id, payee_id, period_key, period_start, period_end, seq, minutes, owed_cents,
         amount_cents, ledger_id, detail, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?)",
    )
    .bind(&id)
    .bind(&payee.row.org_id)
    .bind(&payee.row.id)
    .bind(&period.key)
    .bind(&ps)
    .bind(&pe)
    .bind(seq)
    .bind(pay.minutes)
    .bind(pay.cents)
    .bind(amount)
    .bind(detail.to_string())
    .bind(&now_s)
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() == 0 && !super::mutant::on("timesheet_twice") {
        tx.rollback().await?; // written by a concurrent tick: never twice
        return Ok(Ok(None));
    }
    let mut ledger_id = None;
    if amount > 0 {
        let contributor_id: Option<String> = sqlx::query_scalar(
            "SELECT CASE WHEN typeof(c.id) = 'blob' THEN lower(substr(hex(c.id),1,8)||'-'||substr(hex(c.id),9,4)||'-'||substr(hex(c.id),13,4)||'-'||substr(hex(c.id),17,4)||'-'||substr(hex(c.id),21)) ELSE c.id END
             FROM contributors c JOIN payee_keys k ON k.payee_key = lower(c.github_username) WHERE k.payee_id = ? ORDER BY k.payee_key LIMIT 1",
        )
        .bind(&payee.row.id)
        .fetch_optional(&mut *tx)
        .await?;
        let sref = if super::mutant::on("timesheet_twice") { format!("{}#{}", source_ref(&payee.row.id, &period.key, seq), id) } else { source_ref(&payee.row.id, &period.key, seq) };
        let src = SourceRow {
            source_kind: SOURCE_TIMESHEET,
            source_ref: sref,
            payee_id: payee.row.id.clone(),
            payee_key: payee.ledger_key(),
            contributor_id,
            title: if seq == 0 {
                format!("Timesheet {} ({})", period.key, payee.row.display_name)
            } else {
                format!("Timesheet {} correction {} ({})", period.key, seq, payee.row.display_name)
            },
            amount_cents: amount,
            currency: currency.to_string(),
            pay_after: Some(pe.clone()),
            created_at: now_s.clone(),
        };
        let Some(lid) = ledger::insert_source_row(&mut tx, &src).await? else {
            tx.rollback().await?;
            return Ok(Ok(None));
        };
        sqlx::query("UPDATE timesheet_rows SET ledger_id = ? WHERE id = ?").bind(&lid).bind(&id).execute(&mut *tx).await?;
        ledger_id = Some(lid);
    }
    crate::audit::append_in(
        &mut tx,
        &crate::audit::AuditEntry::new(
            crate::audit::ActorKind::System,
            "system:scheduler",
            if seq == 0 { "timesheet.materialized" } else { "timesheet.correction" },
            format!("payee:{}", payee.row.id),
            json!({
                "payee_id": payee.row.id,
                "period_key": period.key,
                "period_start": ps,
                "period_end": pe,
                "seq": seq,
                "minutes": pay.minutes,
                "owed_cents": pay.cents,
                "amount_cents": amount,
                "leftover_units": pay.leftover_units,
                "units_per_cent": pay.units_per_cent,
                "ledger_id": ledger_id,
            }),
        ),
    )
    .await?;
    tx.commit().await?;
    Ok(Ok(Some((seq, ledger_id))))
}

/// Called from the scheduler tick. Auto-approves closed entries when the policy says so, then
/// writes what every ended period owes each payee with approved time. Creates ledger rows
/// only; never transfers. Safe to call any number of times.
pub async fn materialize_due(ctx: &ApiContext, now: DateTime<Utc>) -> Result<TimesheetReport, PayoutError> {
    let pool = &ctx.pool;
    let mut report = TimesheetReport::default();
    let eff = policy::load_effective(pool, crate::auth::DEFAULT_ORG, &ctx.payments).await?;
    if eff.policy.time_auto_approve {
        report.auto_approved = timeclock::auto_approve_closed(pool, now).await?;
    }
    let horizon = now - Duration::days(LOOKBACK_DAYS);
    let payee_ids: Vec<String> = sqlx::query_scalar("SELECT DISTINCT payee_id FROM time_entries WHERE status = 'approved' AND end_at > ? ORDER BY payee_id")
        .bind(format_db_time(horizon))
        .fetch_all(pool)
        .await?;
    for pid in payee_ids {
        let Some(payee) = payees::load(pool, &pid).await? else { continue };
        let entries: Vec<(String, String)> =
            sqlx::query_as("SELECT start_at, end_at FROM time_entries WHERE payee_id = ? AND status = 'approved' AND end_at > ? ORDER BY start_at")
                .bind(&pid)
                .bind(format_db_time(horizon))
                .fetch_all(pool)
                .await?;
        let mut periods: BTreeMap<String, PeriodSlot> = BTreeMap::new();
        for (s, e) in entries {
            let (Some(s), Some(e)) = (parse_db_time(&s), parse_db_time(&e)) else { continue };
            for p in periods_overlapping(&eff.policy.schedule, s, e.max(s + Duration::seconds(1))) {
                if p.end <= now && p.end > horizon {
                    periods.insert(p.key.clone(), p);
                }
            }
        }
        for (_, period) in periods {
            let pay = compute(pool, ctx.directory.as_ref(), &eff.policy, &pid, &period).await?;
            match write_period(pool, &payee, &eff.policy.currency, &period, &pay, now).await? {
                Ok(Some((seq, lid))) => {
                    info!("timesheet {} seq {} for payee {} written ({} cents)", period.key, seq, pid, pay.cents);
                    if let (true, Some(l)) = (super::mutant::on("timesheet_pays_directly"), &lid) {
                        // The bug this guards: paying a timesheet row outside the payout path.
                        let _ = ledger::approve(pool, ctx.stripe.as_ref(), l, "system:scheduler").await;
                    }
                    report.written.push((pid.clone(), period.key.clone(), seq, lid));
                }
                Ok(None) => {}
                Err(why) => {
                    warn!("timesheet {} for payee {} held: {}", period.key, pid, why);
                    report.held.push((pid.clone(), period.key.clone(), why));
                }
            }
        }
    }
    Ok(report)
}

// ================================================================== the timesheet view

#[derive(Debug, Clone, Serialize)]
pub struct Timesheet {
    pub payee_id: String,
    pub display_name: String,
    pub period_key: String,
    pub period_start: String,
    pub period_end: String,
    pub currency: String,
    /// Every entry overlapping the period, in any state.
    pub entries: Vec<EntryView>,
    /// Minutes per local day of every entry that is not void, by state.
    pub minutes_by_day: Vec<DayMinutes>,
    /// What the approved time in the period owes now: minutes per day, the rate, the amount.
    pub pay: PeriodPay,
    /// The approved rate in force on the period's first and last day.
    pub rate_cents_per_hour_at_start: Option<i64>,
    pub rate_cents_per_hour_at_end: Option<i64>,
    /// Rows already written to the ledger for this period (first row, then corrections).
    pub rows: Vec<TimesheetRow>,
    pub written_cents: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DayMinutes {
    pub date: String,
    pub open_minutes: i64,
    pub closed_minutes: i64,
    pub approved_minutes: i64,
}

/// GET /api/v1/time/timesheet?payee=&period= (`time.read`, or `time.clock` for your own).
/// `period` is a period key or a date; default: the period containing now.
pub async fn timesheet(
    pool: &SqlitePool,
    cfg: &PaymentConfig,
    directory: &dyn Directory,
    principal: &Principal,
    payee: Option<&str>,
    period: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Timesheet, PayoutError> {
    let pid = match timeclock::read_scope(pool, principal, payee).await? {
        Some(p) => p,
        None => return Err(PayoutError::Invalid(vec!["payee is required".into()])),
    };
    let p = payees::load(pool, &pid).await?.ok_or(PayoutError::NotFound("payee"))?;
    if p.row.org_id != principal.org_id {
        return Err(PayoutError::NotFound("payee"));
    }
    let eff = policy::load_effective(pool, &p.row.org_id, cfg).await?;
    let slot = match period.map(str::trim).filter(|s| !s.is_empty()) {
        Some(k) => period_for(&eff.policy.schedule, k).map_err(|e| PayoutError::Invalid(vec![e]))?,
        None => pay_period(&eff.policy.schedule, now),
    };
    let rows = sqlx::query_as::<_, EntryRow>(
        "SELECT * FROM time_entries WHERE payee_id = ? AND start_at < ? AND end_at > ? ORDER BY start_at, id",
    )
    .bind(&pid)
    .bind(format_db_time(slot.end))
    .bind(format_db_time(slot.start))
    .fetch_all(pool)
    .await?;
    let mut by_day: BTreeMap<NaiveDate, DayMinutes> = BTreeMap::new();
    for r in &rows {
        let (Some(s), Some(e), Ok(tz)) = (r.start(), r.end(), shifts::parse_tz(&r.time_zone)) else { continue };
        let state = timeclock::entry_state(&r.status, e, now);
        if state == timeclock::VOID {
            continue;
        }
        let (s, e) = (s.max(slot.start), e.min(slot.end));
        for d in day_segments(s, e, &tz) {
            let line = by_day.entry(d.date).or_insert_with(|| DayMinutes { date: d.date.format("%Y-%m-%d").to_string(), ..Default::default() });
            match state {
                timeclock::APPROVED => line.approved_minutes += d.minutes,
                timeclock::CLOSED => line.closed_minutes += d.minutes,
                _ => line.open_minutes += d.minutes,
            }
        }
    }
    let pay = compute(pool, directory, &eff.policy, &pid, &slot).await?;
    let rates = wages::approved_rates(pool, directory, &eff.policy, &pid).await?;
    let written = sqlx::query_as::<_, TimesheetRow>(
        "SELECT id, payee_id, period_key, period_start, period_end, seq, minutes, owed_cents, amount_cents, ledger_id, detail, created_at
         FROM timesheet_rows WHERE payee_id = ? AND period_key = ? ORDER BY seq",
    )
    .bind(&pid)
    .bind(&slot.key)
    .fetch_all(pool)
    .await?;
    let tz_first = rows.first().and_then(|r| shifts::parse_tz(&r.time_zone).ok());
    let local = |t: DateTime<Utc>| tz_first.as_ref().map(|tz| shifts::local_date(tz, t)).unwrap_or_else(|| t.date_naive());
    Ok(Timesheet {
        payee_id: pid,
        display_name: p.row.display_name.clone(),
        period_key: slot.key.clone(),
        period_start: format_db_time(slot.start),
        period_end: format_db_time(slot.end),
        currency: eff.policy.currency.clone(),
        entries: rows.into_iter().map(|r| timeclock::view(r, now)).collect(),
        minutes_by_day: by_day.into_values().collect(),
        rate_cents_per_hour_at_start: rate_on(&rates, local(slot.start)).map(|r| r.rate_cents_per_hour),
        rate_cents_per_hour_at_end: rate_on(&rates, local(slot.end - Duration::seconds(1))).map(|r| r.rate_cents_per_hour),
        written_cents: written.iter().map(|r| r.amount_cents).sum(),
        rows: written,
        pay,
    })
}
