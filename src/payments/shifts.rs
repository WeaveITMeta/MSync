//! Shifts (contract 14.3): when someone is expected to work.
//!
//! Layout:
//! - Time zone arithmetic is pure and uses IANA zones (`chrono-tz`), so daylight saving is
//!   right: a shift is local wall-clock times in its zone, turned into UTC instants for each
//!   day it occurs. A local time that does not exist (the hour skipped in spring) moves forward
//!   to the first instant that does; a local time that happens twice (the hour repeated in
//!   autumn) takes the first one.
//! - "Which shift occurrence contains this instant, including its early clock-in window" is
//!   pure ([`occurrences_containing`]) with the clock passed in, and so is the clock-in plan
//!   ([`plan_clock_in`]).
//! - Storage: shifts are created and changed by someone with `time.manage`, never by the payee
//!   for themselves. Every create and change is audited in the same transaction.

use super::error::PayoutError;
use super::payees;
use crate::auth::{scopes, tokens::format_db_time, Directory, Principal};
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike, Utc, Weekday};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::SqlitePool;
use uuid::Uuid;

pub const WEEKLY: &str = "weekly";
pub const SINGLE: &str = "single";
pub const OPEN: &str = "open";
pub const PAID_LATER_OF: &str = "later_of_clock_in_and_start";
pub const PAID_CLOCK_IN: &str = "clock_in";
pub const DEFAULT_EARLY_MINUTES: i64 = 15;
/// Longest early clock-in window, and the longest open-shift allowance.
pub const MAX_EARLY_MINUTES: i64 = 240;
pub const MAX_OPEN_MINUTES: i64 = 24 * 60;

// ================================================================== time (pure)

/// Parse an IANA time zone name such as `America/Phoenix`.
pub fn parse_tz(name: &str) -> Result<Tz, String> {
    name.trim().parse::<Tz>().map_err(|_| format!("'{}' is not an IANA time zone name (for example America/New_York)", name.trim()))
}

/// The UTC instant of a local wall-clock time. A time skipped by a daylight saving change
/// becomes the first instant after the gap; a time that occurs twice takes the first one.
pub fn local_to_utc(tz: &Tz, local: NaiveDateTime) -> DateTime<Utc> {
    if let Some(t) = tz.from_local_datetime(&local).earliest() {
        return t.with_timezone(&Utc);
    }
    // In a gap: walk forward a minute at a time (gaps are at most a few hours).
    let mut probe = local;
    for _ in 0..(26 * 60) {
        probe += Duration::minutes(1);
        if let Some(t) = tz.from_local_datetime(&probe).earliest() {
            return t.with_timezone(&Utc);
        }
    }
    // Unreachable for real zones; fall back to reading the local time as UTC.
    Utc.from_utc_datetime(&local)
}

/// Local midnight at the start of `date`, as UTC.
pub fn local_midnight(tz: &Tz, date: NaiveDate) -> DateTime<Utc> {
    local_to_utc(tz, date.and_time(NaiveTime::MIN))
}

/// The local date of an instant.
pub fn local_date(tz: &Tz, t: DateTime<Utc>) -> NaiveDate {
    t.with_timezone(tz).date_naive()
}

/// Round down to the whole minute.
pub fn floor_minute(t: DateTime<Utc>) -> DateTime<Utc> {
    t.with_nanosecond(0).and_then(|x| x.with_second(0)).unwrap_or(t)
}

/// Round up to the whole minute (unchanged when already whole). Paid time starts at a whole
/// minute, so a clock-in at 09:00:20 is paid from 09:01: rounding is always in the payer's
/// direction by less than a minute, never in the payee's.
pub fn ceil_minute(t: DateTime<Utc>) -> DateTime<Utc> {
    let f = floor_minute(t);
    if f == t {
        t
    } else {
        f + Duration::minutes(1)
    }
}

pub fn parse_hhmm(s: &str) -> Result<NaiveTime, String> {
    NaiveTime::parse_from_str(s.trim(), "%H:%M").map_err(|_| format!("'{}' is not a time of day HH:MM", s.trim()))
}

pub fn parse_weekday(s: &str) -> Result<Weekday, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "monday" => Ok(Weekday::Mon),
        "tuesday" => Ok(Weekday::Tue),
        "wednesday" => Ok(Weekday::Wed),
        "thursday" => Ok(Weekday::Thu),
        "friday" => Ok(Weekday::Fri),
        "saturday" => Ok(Weekday::Sat),
        "sunday" => Ok(Weekday::Sun),
        other => Err(format!("'{other}' is not a weekday (monday .. sunday)")),
    }
}

// ================================================================== shift patterns (pure)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PaidFrom {
    /// The default: paid from the shift start or the clock-in, whichever is later.
    LaterOfClockInAndStart,
    /// An early clock-in (inside the window) is paid from the clock-in.
    ClockIn,
}

impl PaidFrom {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            PAID_LATER_OF => Ok(PaidFrom::LaterOfClockInAndStart),
            PAID_CLOCK_IN => Ok(PaidFrom::ClockIn),
            other => Err(format!("paid_from must be {PAID_LATER_OF} or {PAID_CLOCK_IN}, not '{other}'")),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    /// Every week on `weekday`, local `start` to `end` (an end at or before the start is the
    /// next day), within the optional local date range.
    Weekly { weekday: Weekday, start: NaiveTime, end: NaiveTime, valid_from: Option<NaiveDate>, valid_until: Option<NaiveDate> },
    /// One shift on a local date.
    Single { date: NaiveDate, start: NaiveTime, end: NaiveTime },
    /// An open-shift allowance: a clock-in outside every other shift starts an entry of this
    /// many minutes, on a local date in the optional range.
    Open { length_minutes: i64, valid_from: Option<NaiveDate>, valid_until: Option<NaiveDate> },
}

/// A shift ready for the pure functions.
#[derive(Debug, Clone, PartialEq)]
pub struct ShiftSpec {
    pub id: String,
    pub pattern: Pattern,
    pub tz: Tz,
    pub early_minutes: i64,
    pub paid_from: PaidFrom,
}

/// One occurrence of a weekly or single shift.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Occurrence {
    pub shift_id: String,
    /// `<shift id>:<local date>`: one time entry per occurrence.
    pub key: String,
    pub local_date: NaiveDate,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// start minus the early clock-in window.
    pub window_start: DateTime<Utc>,
    #[serde(skip)]
    pub paid_from: PaidFrom,
}

fn in_range(d: NaiveDate, from: Option<NaiveDate>, until: Option<NaiveDate>) -> bool {
    from.is_none_or(|f| d >= f) && until.is_none_or(|u| d <= u)
}

fn bounds(tz: &Tz, date: NaiveDate, start: NaiveTime, end: NaiveTime) -> (DateTime<Utc>, DateTime<Utc>) {
    let s = local_to_utc(tz, date.and_time(start));
    let end_date = if end <= start { date.succ_opt().unwrap_or(date) } else { date };
    if super::mutant::on("naive_dst") {
        // The bug this guards: wall-clock length instead of real elapsed time.
        return (s, s + (end_date.and_time(end) - date.and_time(start)));
    }
    let e = local_to_utc(tz, end_date.and_time(end));
    (s, e.max(s))
}

/// The occurrence of a weekly or single shift that starts on local `date`, if any.
pub fn occurrence_on(spec: &ShiftSpec, date: NaiveDate) -> Option<Occurrence> {
    let (start, end) = match &spec.pattern {
        Pattern::Weekly { weekday, start, end, valid_from, valid_until } => {
            if date.weekday() != *weekday || !in_range(date, *valid_from, *valid_until) {
                return None;
            }
            bounds(&spec.tz, date, *start, *end)
        }
        Pattern::Single { date: d, start, end } => {
            if *d != date {
                return None;
            }
            bounds(&spec.tz, date, *start, *end)
        }
        Pattern::Open { .. } => return None,
    };
    Some(Occurrence {
        shift_id: spec.id.clone(),
        key: format!("{}:{}", spec.id, date.format("%Y-%m-%d")),
        local_date: date,
        start,
        end,
        window_start: if super::mutant::on("no_early_window") { start } else { start - Duration::minutes(spec.early_minutes.max(0)) },
        paid_from: spec.paid_from,
    })
}

fn nearby_dates(tz: &Tz, now: DateTime<Utc>, back: i64, forward: i64) -> Vec<NaiveDate> {
    let today = local_date(tz, now);
    (-back..=forward).filter_map(|i| today.checked_add_signed(Duration::days(i))).collect()
}

/// Pure: every weekly or single occurrence whose window (early clock-in window included)
/// contains `now`: `window_start <= now < end`. Earliest start first, then by shift id.
pub fn occurrences_containing(specs: &[ShiftSpec], now: DateTime<Utc>) -> Vec<Occurrence> {
    let mut out = Vec::new();
    for s in specs {
        for d in nearby_dates(&s.tz, now, 2, 1) {
            if let Some(o) = occurrence_on(s, d) {
                let open = if super::mutant::on("window_open_all_day") { true } else { o.window_start <= now };
                if open && (now < o.end || super::mutant::on("after_end_ok")) {
                    out.push(o);
                }
            }
        }
    }
    out.sort_by(|a, b| (a.start, &a.shift_id).cmp(&(b.start, &b.shift_id)));
    out
}

/// Pure: the occurrence in progress at `now` (window included), or else the next one to
/// start within 14 days. For the chat `shift` command.
pub fn current_or_next(specs: &[ShiftSpec], now: DateTime<Utc>) -> Option<Occurrence> {
    if let Some(o) = occurrences_containing(specs, now).into_iter().next() {
        return Some(o);
    }
    let mut best: Option<Occurrence> = None;
    for s in specs {
        for d in nearby_dates(&s.tz, now, 0, 14) {
            if let Some(o) = occurrence_on(s, d) {
                if o.window_start > now && best.as_ref().is_none_or(|b| (o.start, &o.shift_id) < (b.start, &b.shift_id)) {
                    best = Some(o);
                }
            }
        }
    }
    best
}

/// What a clock-in at `now` would create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockInPlan {
    pub shift_id: String,
    pub occurrence_key: String,
    /// The shift occurrence's own start and end (for an open-shift entry: the entry's).
    pub shift_start: DateTime<Utc>,
    pub shift_end: DateTime<Utc>,
    /// The entry: whole minutes. `end` is the shift's end, fixed now.
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub open_shift: bool,
}

/// Why a clock-in is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClockInRefusal {
    /// No shift window contains now and there is no open-shift allowance.
    NoShift,
    /// A shift today has already ended (and there is no open-shift allowance).
    AfterEnd { shift_id: String, ended_at: DateTime<Utc> },
}

impl ClockInRefusal {
    pub fn message(&self) -> String {
        match self {
            ClockInRefusal::NoShift => "there is no shift to clock in to right now (not even inside an early clock-in window), and there is no open-shift allowance".into(),
            ClockInRefusal::AfterEnd { ended_at, .. } => {
                format!("the shift ended at {} UTC; a clock-in after a shift's end is refused", format_db_time(*ended_at))
            }
        }
    }
}

/// The entry bounds for a clock-in at `now` into `occ`: start never before the shift start
/// (unless the shift pays from clock-in), whole minutes, end = the shift's end. None when no
/// whole minute is left before the end.
pub fn entry_bounds(occ: &Occurrence, now: DateTime<Utc>) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let clock = ceil_minute(now);
    let start = match occ.paid_from {
        PaidFrom::LaterOfClockInAndStart if super::mutant::on("start_before_shift") => clock,
        PaidFrom::LaterOfClockInAndStart => clock.max(occ.start),
        PaidFrom::ClockIn => clock.max(occ.window_start),
    };
    let end = if super::mutant::on("end_not_shift_end") {
        start + Duration::hours(8)
    } else if super::mutant::on("after_end_ok") {
        occ.end.max(start + Duration::hours(1))
    } else {
        occ.end
    };
    (start < end).then_some((start, end))
}

/// Pure: the clock-in plan at `now` for a payee's shifts. A weekly or single shift whose
/// window contains `now` wins (the earliest one); otherwise an open-shift allowance valid
/// today starts an entry of its length, cut short where the next scheduled shift starts;
/// otherwise the clock-in is refused (after the end, or no shift).
pub fn plan_clock_in(specs: &[ShiftSpec], now: DateTime<Utc>) -> Result<ClockInPlan, ClockInRefusal> {
    for occ in occurrences_containing(specs, now) {
        if let Some((start, end)) = entry_bounds(&occ, now) {
            return Ok(ClockInPlan {
                shift_id: occ.shift_id.clone(),
                occurrence_key: occ.key.clone(),
                shift_start: occ.start,
                shift_end: occ.end,
                start,
                end,
                open_shift: false,
            });
        }
    }
    // An open-shift allowance valid on today's local date.
    let mut open: Vec<&ShiftSpec> = specs.iter().filter(|s| matches!(s.pattern, Pattern::Open { .. })).collect();
    open.sort_by(|a, b| a.id.cmp(&b.id));
    for s in open {
        let Pattern::Open { length_minutes, valid_from, valid_until } = &s.pattern else { continue };
        if !in_range(local_date(&s.tz, now), *valid_from, *valid_until) || *length_minutes <= 0 {
            continue;
        }
        let start = ceil_minute(now);
        let mut end = start + Duration::minutes(*length_minutes);
        // Never run into a scheduled shift: stop where the next one starts.
        for other in specs {
            for d in nearby_dates(&other.tz, now, 1, 2) {
                if let Some(o) = occurrence_on(other, d) {
                    if o.start >= start && o.start < end {
                        end = o.start;
                    }
                }
            }
        }
        if start < end {
            return Ok(ClockInPlan {
                shift_id: s.id.clone(),
                occurrence_key: format!("{OPEN}:{}:{}", s.id, start.format("%Y-%m-%dT%H:%MZ")),
                shift_start: start,
                shift_end: end,
                start,
                end,
                open_shift: true,
            });
        }
    }
    if super::mutant::on("no_shift_ok") {
        // The bug this guards: a clock-in with no shift starting an entry anyway.
        let start = ceil_minute(now);
        let end = start + Duration::hours(1);
        return Ok(ClockInPlan { shift_id: specs.first().map(|s| s.id.clone()).unwrap_or_default(), occurrence_key: format!("none:{start}"), shift_start: start, shift_end: end, start, end, open_shift: true });
    }
    // Refused: name a shift that already ended today, when there is one.
    let mut ended: Option<(String, DateTime<Utc>)> = None;
    for s in specs {
        for d in nearby_dates(&s.tz, now, 1, 0) {
            if let Some(o) = occurrence_on(s, d) {
                if o.end <= now && ended.as_ref().is_none_or(|(_, e)| o.end > *e) && (now - o.end) < Duration::hours(24) {
                    ended = Some((o.shift_id.clone(), o.end));
                }
            }
        }
    }
    Err(match ended {
        Some((shift_id, ended_at)) => ClockInRefusal::AfterEnd { shift_id, ended_at },
        None => ClockInRefusal::NoShift,
    })
}

/// One piece of worked time inside one local day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DaySegment {
    pub date: NaiveDate,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub minutes: i64,
}

/// Pure: split [start, end) at each local midnight in `tz`. Minutes are whole minutes of real
/// elapsed time, so a day with a daylight saving change counts what was actually worked.
pub fn day_segments(start: DateTime<Utc>, end: DateTime<Utc>, tz: &Tz) -> Vec<DaySegment> {
    let mut out = Vec::new();
    let mut cur = start;
    let mut guard = 0;
    while cur < end && guard < 400 {
        guard += 1;
        let d = local_date(tz, cur);
        let next = d.succ_opt().map(|n| local_midnight(tz, n)).unwrap_or(end);
        let seg_end = if next > cur { next.min(end) } else { end };
        out.push(DaySegment { date: d, start: cur, end: seg_end, minutes: (seg_end - cur).num_seconds() / 60 });
        cur = seg_end;
    }
    out
}

// ================================================================== storage

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ShiftRow {
    pub id: String,
    pub org_id: String,
    pub payee_id: String,
    pub kind: String,
    pub weekday: Option<String>,
    pub date: Option<String>,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub length_minutes: Option<i64>,
    pub time_zone: String,
    pub valid_from: Option<String>,
    pub valid_until: Option<String>,
    pub early_minutes: i64,
    pub paid_from: String,
    pub status: String,
    pub created_by: String,
    pub created_at: String,
    pub updated_by: String,
    pub updated_at: String,
}

fn opt_date(s: &Option<String>, field: &str, errs: &mut Vec<String>) -> Option<NaiveDate> {
    s.as_deref().and_then(|v| match NaiveDate::parse_from_str(v, "%Y-%m-%d") {
        Ok(d) => Some(d),
        Err(_) => {
            errs.push(format!("{field} must be a date YYYY-MM-DD"));
            None
        }
    })
}

impl ShiftRow {
    /// The pure form, or every reason it is not valid.
    pub fn spec(&self) -> Result<ShiftSpec, Vec<String>> {
        let mut errs = Vec::new();
        let tz = parse_tz(&self.time_zone).map_err(|e| errs.push(e)).ok();
        let paid_from = PaidFrom::parse(&self.paid_from).map_err(|e| errs.push(e)).ok();
        if !(0..=MAX_EARLY_MINUTES).contains(&self.early_minutes) {
            errs.push(format!("early_minutes must be 0 to {MAX_EARLY_MINUTES}"));
        }
        let valid_from = opt_date(&self.valid_from, "valid_from", &mut errs);
        let valid_until = opt_date(&self.valid_until, "valid_until", &mut errs);
        if let (Some(a), Some(b)) = (valid_from, valid_until) {
            if b < a {
                errs.push("valid_until must be on or after valid_from".into());
            }
        }
        let times = |errs: &mut Vec<String>| -> Option<(NaiveTime, NaiveTime)> {
            let s = self.start_time.as_deref().map(parse_hhmm);
            let e = self.end_time.as_deref().map(parse_hhmm);
            match (s, e) {
                (Some(Ok(s)), Some(Ok(e))) => Some((s, e)),
                (Some(Err(m)), _) | (_, Some(Err(m))) => {
                    errs.push(m);
                    None
                }
                _ => {
                    errs.push("start_time and end_time (HH:MM) are required".into());
                    None
                }
            }
        };
        let pattern = match self.kind.as_str() {
            WEEKLY => {
                let wd = match self.weekday.as_deref().map(parse_weekday) {
                    Some(Ok(w)) => Some(w),
                    Some(Err(m)) => {
                        errs.push(m);
                        None
                    }
                    None => {
                        errs.push("weekday is required for a weekly shift".into());
                        None
                    }
                };
                match (wd, times(&mut errs)) {
                    (Some(weekday), Some((start, end))) => Some(Pattern::Weekly { weekday, start, end, valid_from, valid_until }),
                    _ => None,
                }
            }
            SINGLE => {
                let d = match &self.date {
                    Some(_) => opt_date(&self.date, "date", &mut errs),
                    None => {
                        errs.push("date is required for a single shift".into());
                        None
                    }
                };
                if self.valid_from.is_some() || self.valid_until.is_some() {
                    errs.push("valid_from and valid_until apply to weekly and open shifts only".into());
                }
                match (d, times(&mut errs)) {
                    (Some(date), Some((start, end))) => Some(Pattern::Single { date, start, end }),
                    _ => None,
                }
            }
            OPEN => match self.length_minutes {
                Some(l) if (1..=MAX_OPEN_MINUTES).contains(&l) => {
                    if self.start_time.is_some() || self.end_time.is_some() || self.weekday.is_some() || self.date.is_some() {
                        errs.push("an open shift has only length_minutes (and an optional valid_from and valid_until)".into());
                    }
                    Some(Pattern::Open { length_minutes: l, valid_from, valid_until })
                }
                _ => {
                    errs.push(format!("length_minutes (1 to {MAX_OPEN_MINUTES}) is required for an open shift"));
                    None
                }
            },
            other => {
                errs.push(format!("kind must be weekly, single or open, not '{other}'"));
                None
            }
        };
        if self.kind != OPEN && self.length_minutes.is_some() {
            errs.push("length_minutes applies to open shifts only".into());
        }
        match (errs.is_empty(), pattern, tz, paid_from) {
            (true, Some(pattern), Some(tz), Some(paid_from)) => {
                Ok(ShiftSpec { id: self.id.clone(), pattern, tz, early_minutes: self.early_minutes, paid_from })
            }
            _ => Err(errs),
        }
    }
}

/// POST /api/v1/time/shifts. The payee is named here because a manager creates shifts for
/// someone else; the creator is always the token's member.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewShift {
    pub payee_id: String,
    /// weekly | single | open
    pub kind: String,
    #[serde(default)]
    pub weekday: Option<String>,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub start_time: Option<String>,
    #[serde(default)]
    pub end_time: Option<String>,
    #[serde(default)]
    pub length_minutes: Option<i64>,
    pub time_zone: String,
    #[serde(default)]
    pub valid_from: Option<String>,
    #[serde(default)]
    pub valid_until: Option<String>,
    #[serde(default)]
    pub early_minutes: Option<i64>,
    #[serde(default)]
    pub paid_from: Option<String>,
}

/// PATCH /api/v1/time/shifts/{id}. Absent fields are unchanged; `null` clears an optional
/// one. The payee and the kind never change. Entries already clocked keep their own times.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShiftPatch {
    #[serde(default)]
    pub weekday: Option<String>,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub start_time: Option<String>,
    #[serde(default)]
    pub end_time: Option<String>,
    #[serde(default)]
    pub length_minutes: Option<i64>,
    #[serde(default)]
    pub time_zone: Option<String>,
    #[serde(default, with = "payees::double_option")]
    pub valid_from: Option<Option<String>>,
    #[serde(default, with = "payees::double_option")]
    pub valid_until: Option<Option<String>>,
    #[serde(default)]
    pub early_minutes: Option<i64>,
    #[serde(default)]
    pub paid_from: Option<String>,
    /// active | ended
    #[serde(default)]
    pub status: Option<String>,
}

fn clean(s: &Option<String>) -> Option<String> {
    s.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string)
}

pub async fn load(pool: &SqlitePool, id: &str) -> Result<Option<ShiftRow>, sqlx::Error> {
    sqlx::query_as::<_, ShiftRow>("SELECT * FROM shifts WHERE id = ?").bind(id).fetch_optional(pool).await
}

/// The active shifts of a payee that parse (an unparsable stored shift is logged and skipped).
pub async fn active_specs(pool: &SqlitePool, payee_id: &str) -> Result<Vec<ShiftSpec>, sqlx::Error> {
    let rows = sqlx::query_as::<_, ShiftRow>("SELECT * FROM shifts WHERE payee_id = ? AND status = 'active' ORDER BY created_at, id")
        .bind(payee_id)
        .fetch_all(pool)
        .await?;
    let mut out = Vec::new();
    for r in rows {
        match r.spec() {
            Ok(s) => out.push(s),
            Err(e) => tracing::error!("shift {} is unreadable and is skipped: {}", r.id, e.join("; ")),
        }
    }
    Ok(out)
}

async fn refuse_own(pool: &SqlitePool, directory: &dyn Directory, principal: &Principal, payee_id: &str) -> Result<(), PayoutError> {
    if payees::principal_is_payee_of(pool, directory, principal, payee_id).await? && !super::mutant::on("payee_creates_own_shift") {
        return Err(PayoutError::Forbidden("a payee cannot create or change their own shifts".into()));
    }
    Ok(())
}

/// GET /api/v1/time/shifts (`time.read`; with only `time.clock`, the caller's own shifts).
pub async fn list(
    pool: &SqlitePool,
    principal: &Principal,
    payee_id: Option<&str>,
    status: Option<&str>,
) -> Result<Vec<ShiftRow>, PayoutError> {
    let payee = if super::timeclock::has(principal, scopes::TIME_READ) {
        payee_id.map(str::to_string)
    } else if super::timeclock::has(principal, scopes::TIME_CLOCK) {
        let own = super::timeclock::own_payee(pool, principal).await?;
        if payee_id.is_some_and(|p| p != own.row.id) {
            return Err(PayoutError::Forbidden("with time.clock only, you can read only your own shifts".into()));
        }
        Some(own.row.id)
    } else {
        super::timeclock::need(principal, scopes::TIME_READ)?;
        None
    };
    let rows = sqlx::query_as::<_, ShiftRow>(
        "SELECT * FROM shifts WHERE org_id = ? AND (? IS NULL OR payee_id = ?) AND (? IS NULL OR status = ?) ORDER BY payee_id, created_at, id",
    )
    .bind(&principal.org_id)
    .bind(&payee)
    .bind(&payee)
    .bind(status)
    .bind(status)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

async fn audit_shift(conn: &mut sqlx::SqliteConnection, principal: &Principal, action: &str, row: &ShiftRow, extra: serde_json::Value) -> Result<(), sqlx::Error> {
    crate::audit::append_in(
        conn,
        &crate::audit::AuditEntry::by_principal(
            principal,
            action,
            format!("shift:{}", row.id),
            json!({ "shift_id": row.id, "payee_id": row.payee_id, "shift": row, "change": extra }),
        ),
    )
    .await?;
    Ok(())
}

/// POST /api/v1/time/shifts (`time.manage`). Never by the payee for themselves.
pub async fn create(
    pool: &SqlitePool,
    directory: &dyn Directory,
    principal: &Principal,
    input: NewShift,
    now: DateTime<Utc>,
) -> Result<ShiftRow, PayoutError> {
    super::timeclock::need(principal, scopes::TIME_MANAGE)?;
    let payee = payees::load(pool, input.payee_id.trim()).await?.ok_or(PayoutError::NotFound("payee"))?;
    if payee.row.org_id != principal.org_id {
        return Err(PayoutError::NotFound("payee"));
    }
    refuse_own(pool, directory, principal, &payee.row.id).await?;
    if payee.row.status == "closed" {
        return Err(PayoutError::InvalidState("the payee is closed".into()));
    }
    let now_s = format_db_time(now);
    let row = ShiftRow {
        id: Uuid::new_v4().to_string(),
        org_id: principal.org_id.clone(),
        payee_id: payee.row.id.clone(),
        kind: input.kind.trim().to_string(),
        weekday: clean(&input.weekday).map(|w| w.to_ascii_lowercase()),
        date: clean(&input.date),
        start_time: clean(&input.start_time),
        end_time: clean(&input.end_time),
        length_minutes: input.length_minutes,
        time_zone: input.time_zone.trim().to_string(),
        valid_from: clean(&input.valid_from),
        valid_until: clean(&input.valid_until),
        early_minutes: input.early_minutes.unwrap_or(DEFAULT_EARLY_MINUTES),
        paid_from: clean(&input.paid_from).unwrap_or_else(|| PAID_LATER_OF.to_string()),
        status: "active".into(),
        created_by: principal.member_id.clone(),
        created_at: now_s.clone(),
        updated_by: principal.member_id.clone(),
        updated_at: now_s,
    };
    row.spec().map_err(PayoutError::Invalid)?;
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO shifts (id, org_id, payee_id, kind, weekday, date, start_time, end_time, length_minutes, time_zone,
         valid_from, valid_until, early_minutes, paid_from, status, created_by, created_at, updated_by, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&row.id)
    .bind(&row.org_id)
    .bind(&row.payee_id)
    .bind(&row.kind)
    .bind(&row.weekday)
    .bind(&row.date)
    .bind(&row.start_time)
    .bind(&row.end_time)
    .bind(row.length_minutes)
    .bind(&row.time_zone)
    .bind(&row.valid_from)
    .bind(&row.valid_until)
    .bind(row.early_minutes)
    .bind(&row.paid_from)
    .bind(&row.status)
    .bind(&row.created_by)
    .bind(&row.created_at)
    .bind(&row.updated_by)
    .bind(&row.updated_at)
    .execute(&mut *tx)
    .await?;
    audit_shift(&mut tx, principal, "shift.created", &row, json!({})).await?;
    tx.commit().await?;
    Ok(row)
}

/// PATCH /api/v1/time/shifts/{id} (`time.manage`). Never by the payee.
pub async fn update(
    pool: &SqlitePool,
    directory: &dyn Directory,
    principal: &Principal,
    id: &str,
    patch: ShiftPatch,
    now: DateTime<Utc>,
) -> Result<ShiftRow, PayoutError> {
    super::timeclock::need(principal, scopes::TIME_MANAGE)?;
    let cur = load(pool, id).await?.ok_or(PayoutError::NotFound("shift"))?;
    if cur.org_id != principal.org_id {
        return Err(PayoutError::NotFound("shift"));
    }
    refuse_own(pool, directory, principal, &cur.payee_id).await?;
    let mut next = cur.clone();
    let mut changed: Vec<&str> = Vec::new();
    macro_rules! set {
        ($f:ident, $v:expr) => {
            if let Some(v) = $v {
                changed.push(stringify!($f));
                next.$f = v;
            }
        };
    }
    set!(weekday, patch.weekday.as_ref().map(|w| Some(w.trim().to_ascii_lowercase())));
    set!(date, patch.date.as_ref().map(|d| Some(d.trim().to_string())));
    set!(start_time, patch.start_time.as_ref().map(|t| Some(t.trim().to_string())));
    set!(end_time, patch.end_time.as_ref().map(|t| Some(t.trim().to_string())));
    set!(length_minutes, patch.length_minutes.map(Some));
    set!(time_zone, patch.time_zone.as_ref().map(|t| t.trim().to_string()));
    set!(valid_from, patch.valid_from.clone().map(|v| clean(&v)));
    set!(valid_until, patch.valid_until.clone().map(|v| clean(&v)));
    set!(early_minutes, patch.early_minutes);
    set!(paid_from, patch.paid_from.as_ref().map(|p| p.trim().to_string()));
    set!(status, patch.status.as_ref().map(|s| s.trim().to_string()));
    if next.status != "active" && next.status != "ended" {
        return Err(PayoutError::Invalid(vec!["status must be active or ended".into()]));
    }
    if cur.status == "ended" && next.status == "active" {
        return Err(PayoutError::Invalid(vec!["an ended shift cannot be reopened; create a new shift".into()]));
    }
    next.spec().map_err(PayoutError::Invalid)?;
    let now_s = format_db_time(now);
    next.updated_by = principal.member_id.clone();
    next.updated_at = now_s.clone();
    let mut tx = pool.begin().await?;
    let res = sqlx::query(
        "UPDATE shifts SET weekday = ?, date = ?, start_time = ?, end_time = ?, length_minutes = ?, time_zone = ?, valid_from = ?,
         valid_until = ?, early_minutes = ?, paid_from = ?, status = ?, updated_by = ?, updated_at = ? WHERE id = ? AND updated_at = ?",
    )
    .bind(&next.weekday)
    .bind(&next.date)
    .bind(&next.start_time)
    .bind(&next.end_time)
    .bind(next.length_minutes)
    .bind(&next.time_zone)
    .bind(&next.valid_from)
    .bind(&next.valid_until)
    .bind(next.early_minutes)
    .bind(&next.paid_from)
    .bind(&next.status)
    .bind(&next.updated_by)
    .bind(&next.updated_at)
    .bind(id)
    .bind(&cur.updated_at)
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() == 0 {
        return Err(PayoutError::InvalidState("shift changed concurrently; read it again".into()));
    }
    audit_shift(&mut tx, principal, "shift.changed", &next, json!({ "fields": changed, "before": cur })).await?;
    tx.commit().await?;
    Ok(next)
}
