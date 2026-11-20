//! Hourly pay and time tests (contract 14): shifts and time zones (daylight saving both ways,
//! and America/Phoenix), clocking in, authority, period pay, the exactly-once timesheet row,
//! corrections, and the REST surface. Stripe is the mock from `tests.rs`; nothing here calls
//! live Stripe.
//!
//! Mutations: `MSYNC_TIME_MUTANT=<name>` switches one deliberate bug on (`super::mutant`);
//! each test names the mutants it must catch.

use super::api::{self, ApiContext};
use super::approvals;
use super::error::PayoutError;
use super::ledger::{self, SOURCE_TIMESHEET};
use super::payees;
use super::policy::{self, PayoutPolicy};
use super::runs::{self, CreateRunResult};
use super::schedule;
use super::shifts::{self, day_segments, occurrence_on, parse_tz, plan_clock_in, ClockInRefusal, PaidFrom, Pattern, ShiftSpec};
use super::statement;
use super::tests::{add_contributor, cfg, pool, MockStripe};
use super::timeclock::{self, AdjustBody, EntryFilter};
use super::timesheet::{self, period_pay, WorkSegment};
use super::wages::{self, rate_on, NewWage, WageRate};
use crate::auth::{create_token, scopes, EnvDirectory, NewToken, Principal, TokenCreator};
use actix_web::{test as atest, web, App};
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, TimeZone, Utc, Weekday};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::sync::Arc;

// ================================================================== helpers

fn dir() -> EnvDirectory {
    EnvDirectory::new(vec!["owner1".to_string(), "owner2".to_string(), "owner3".to_string()])
}

const ALL_TIME: &[&str] = &[
    scopes::TIME_CLOCK,
    scopes::TIME_READ,
    scopes::TIME_MANAGE,
    scopes::TIME_APPROVE,
    scopes::PAYEES_READ,
    scopes::PAYEES_MANAGE,
    scopes::PAYORDERS_APPROVE,
    scopes::PAYOUTS_READ,
    scopes::PAYOUTS_APPROVE,
];

fn who(member: &str) -> Principal {
    Principal {
        token_id: format!("tok-{member}"),
        org_id: "default".into(),
        member_id: member.into(),
        scopes: ALL_TIME.iter().map(|s| s.to_string()).collect(),
        payee_keys: vec![],
    }
}

fn utc(y: i32, m: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, h, mi, s).unwrap()
}

fn date(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
}

fn hm(h: u32, m: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(h, m, 0).unwrap()
}

fn weekly(id: &str, weekday: Weekday, start: NaiveTime, end: NaiveTime, tz: &str) -> ShiftSpec {
    ShiftSpec {
        id: id.into(),
        pattern: Pattern::Weekly { weekday, start, end, valid_from: None, valid_until: None },
        tz: parse_tz(tz).unwrap(),
        early_minutes: 15,
        paid_from: PaidFrom::LaterOfClockInAndStart,
    }
}

fn single(id: &str, d: &str, start: NaiveTime, end: NaiveTime, tz: &str) -> ShiftSpec {
    ShiftSpec {
        id: id.into(),
        pattern: Pattern::Single { date: date(d), start, end },
        tz: parse_tz(tz).unwrap(),
        early_minutes: 15,
        paid_from: PaidFrom::LaterOfClockInAndStart,
    }
}

fn mins(a: DateTime<Utc>, b: DateTime<Utc>) -> i64 {
    (b - a).num_minutes()
}

/// Weekly periods, Monday 00:00 UTC; one approval at rank 0 or above.
fn weekly_policy() -> PayoutPolicy {
    serde_json::from_value(json!({
        "schema": 1, "enabled": true, "currency": "usd",
        "schedule": {"cadence": "weekly", "day": "monday", "hour_utc": 0},
        "limits": {},
        "approval": {"approvals_required": 1, "min_approver_rank": 0},
        "rate": [{"branch": "main", "base_cents": 2000, "merit_multiplier": 0.01}]
    }))
    .unwrap()
}

async fn store(pool: &SqlitePool, p: &PayoutPolicy) {
    policy::put_policy(pool, &Principal::legacy_admin(), p, None).await.unwrap();
}

/// A payee created by staff, with a Stripe account the webhook has enabled.
async fn payee(pool: &SqlitePool, name: &str, member: &str) -> String {
    let key = name.to_lowercase();
    let p = payees::create(
        pool,
        &dir(),
        &who("staff"),
        serde_json::from_value(json!({
            "display_name": name, "kind": "individual", "payee_keys": [key], "member_id": member,
            "stripe_account_id": format!("acct_{key}"),
        }))
        .unwrap(),
        Utc::now(),
    )
    .await
    .unwrap();
    ledger::update_account_status(pool, &format!("acct_{key}"), true).await.unwrap();
    p.row.id
}

async fn shift(pool: &SqlitePool, by: &str, body: Value) -> Result<shifts::ShiftRow, PayoutError> {
    shifts::create(pool, &dir(), &who(by), serde_json::from_value(body).unwrap(), Utc::now()).await
}

/// A wage set by staff and approved by owner1 (or left unapproved).
async fn wage(pool: &SqlitePool, payee_id: &str, cents: i64, from: &str, approve: bool) -> String {
    let w = wages::set(
        pool,
        &cfg(),
        &dir(),
        &who("staff"),
        payee_id,
        NewWage { rate_cents_per_hour: cents, effective_from: date(from), currency: None, overtime_after_minutes: None, overtime_bps: None, memo: None },
        Utc::now(),
    )
    .await
    .unwrap();
    if approve {
        wages::approve(pool, &cfg(), &dir(), &who("owner1"), payee_id, &w.row.id, Utc::now()).await.unwrap();
    }
    w.row.id
}

/// An entry written straight to the store (for pay tests).
async fn entry(pool: &SqlitePool, payee_id: &str, start: DateTime<Utc>, end: DateTime<Utc>, tz: &str, status: &str) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let (s, e) = (crate::auth::tokens::format_db_time(start), crate::auth::tokens::format_db_time(end));
    sqlx::query(
        "INSERT INTO time_entries (id, org_id, payee_id, shift_id, occurrence_key, time_zone, clock_in_at, shift_start_at, shift_end_at,
         start_at, end_at, status, approved_by, approved_at, clocked_in_by, created_at, updated_at)
         VALUES (?, 'default', ?, 'test-shift', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'test', ?, ?)",
    )
    .bind(&id)
    .bind(payee_id)
    .bind(format!("test:{id}"))
    .bind(tz)
    .bind(&s)
    .bind(&s)
    .bind(&e)
    .bind(&s)
    .bind(&e)
    .bind(status)
    .bind((status == "approved").then_some("owner1"))
    .bind((status == "approved").then(|| e.clone()))
    .bind(&s)
    .bind(&s)
    .execute(pool)
    .await
    .unwrap();
    id
}

fn ctx(pool: &SqlitePool, stripe: Arc<MockStripe>) -> ApiContext {
    ApiContext { pool: pool.clone(), payments: Arc::new(cfg()), stripe, directory: Arc::new(dir()) }
}

async fn count(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

async fn timesheet_rows(pool: &SqlitePool) -> Vec<ledger::LedgerEntry> {
    sqlx::query_as::<_, ledger::LedgerEntry>("SELECT * FROM payout_ledger WHERE source_kind = 'timesheet' ORDER BY created_at, source_ref")
        .fetch_all(pool)
        .await
        .unwrap()
}

fn seg(d: &str, start: DateTime<Utc>, minutes: i64) -> WorkSegment {
    WorkSegment { entry_id: format!("e-{d}"), date: date(d), start, end: start + Duration::minutes(minutes), minutes }
}

fn rate(id: &str, from: &str, cents: i64) -> WageRate {
    WageRate { id: id.into(), effective_from: date(from), created_at: "2026-01-01 00:00:00".into(), rate_cents_per_hour: cents, overtime_after_minutes: None, overtime_bps: None }
}

// ================================================================== shifts and clock-in (pure)

/// Mutants: start_before_shift, end_not_shift_end.
#[test]
fn a_clock_in_never_starts_before_the_shift_and_ends_at_the_shift_end() {
    let s = single("s1", "2026-11-10", hm(9, 0), hm(17, 0), "America/Phoenix");
    let start = utc(2026, 11, 10, 16, 0, 0); // 09:00 MST
    let end = utc(2026, 11, 11, 0, 0, 0); // 17:00 MST
    // early, inside the window: paid from the shift start
    let p = plan_clock_in(&[s.clone()], utc(2026, 11, 10, 15, 50, 30)).unwrap();
    assert_eq!((p.start, p.end), (start, end));
    // late: paid from the clock-in, rounded up to the whole minute; the end does not move
    let p = plan_clock_in(&[s.clone()], utc(2026, 11, 10, 16, 10, 20)).unwrap();
    assert_eq!((p.start, p.end), (utc(2026, 11, 10, 16, 11, 0), end));
    assert_eq!(p.occurrence_key, "s1:2026-11-10");
    // a shift that pays from clock-in pays the early minutes (never before the window)
    let mut c = s.clone();
    c.paid_from = PaidFrom::ClockIn;
    let p = plan_clock_in(&[c], utc(2026, 11, 10, 15, 50, 30)).unwrap();
    assert_eq!((p.start, p.end), (utc(2026, 11, 10, 15, 51, 0), end));
}

/// Mutants: no_early_window, window_open_all_day, after_end_ok, no_shift_ok.
#[test]
fn the_early_window_is_honoured_and_clock_ins_outside_it_or_after_the_end_are_refused() {
    let s = single("s1", "2026-11-10", hm(9, 0), hm(17, 0), "America/Phoenix");
    // 08:50 local: inside the 15 minute window
    assert!(plan_clock_in(&[s.clone()], utc(2026, 11, 10, 15, 50, 0)).is_ok());
    // 08:45 local exactly: the window's first instant
    assert!(plan_clock_in(&[s.clone()], utc(2026, 11, 10, 15, 45, 0)).is_ok());
    // 08:44 local: before the window
    assert_eq!(plan_clock_in(&[s.clone()], utc(2026, 11, 10, 15, 44, 0)), Err(ClockInRefusal::NoShift));
    // two hours early
    assert_eq!(plan_clock_in(&[s.clone()], utc(2026, 11, 10, 14, 0, 0)), Err(ClockInRefusal::NoShift));
    // at the end, and after it
    for t in [utc(2026, 11, 11, 0, 0, 0), utc(2026, 11, 11, 1, 30, 0)] {
        assert!(matches!(plan_clock_in(&[s.clone()], t), Err(ClockInRefusal::AfterEnd { .. })), "{t}");
    }
    // in the last half minute: no whole minute left
    assert!(plan_clock_in(&[s.clone()], utc(2026, 11, 10, 23, 59, 40)).is_err());
    // no shift at all
    assert_eq!(plan_clock_in(&[], utc(2026, 11, 10, 16, 0, 0)), Err(ClockInRefusal::NoShift));
    // a different day
    assert_eq!(plan_clock_in(&[s], utc(2026, 11, 12, 16, 0, 0)), Err(ClockInRefusal::NoShift));
}

#[test]
fn an_open_shift_allowance_starts_an_entry_of_its_length_and_stops_at_the_next_shift() {
    let open = ShiftSpec {
        id: "o1".into(),
        pattern: Pattern::Open { length_minutes: 240, valid_from: None, valid_until: None },
        tz: parse_tz("America/Phoenix").unwrap(),
        early_minutes: 0,
        paid_from: PaidFrom::LaterOfClockInAndStart,
    };
    let p = plan_clock_in(&[open.clone()], utc(2026, 11, 10, 18, 0, 10)).unwrap();
    assert!(p.open_shift);
    assert_eq!((p.start, mins(p.start, p.end)), (utc(2026, 11, 10, 18, 1, 0), 240));
    // a scheduled shift two hours later cuts it short there
    let s = single("s1", "2026-11-10", hm(13, 0), hm(17, 0), "America/Phoenix"); // 20:00 UTC
    let p = plan_clock_in(&[open, s], utc(2026, 11, 10, 18, 0, 0)).unwrap();
    assert_eq!(p.end, utc(2026, 11, 10, 20, 0, 0));
}

/// Mutant: naive_dst.
#[test]
fn shifts_across_both_daylight_saving_changes_pay_real_minutes_and_phoenix_is_unaffected() {
    let ny = weekly("ny", Weekday::Sat, hm(22, 0), hm(6, 0), "America/New_York");
    let az = weekly("az", Weekday::Sat, hm(22, 0), hm(6, 0), "America/Phoenix");
    // Spring: 2026-03-08 02:00 EST jumps to 03:00 EDT, so the night is 7 hours.
    let o = occurrence_on(&ny, date("2026-03-07")).unwrap();
    assert_eq!((o.start, o.end), (utc(2026, 3, 8, 3, 0, 0), utc(2026, 3, 8, 10, 0, 0)));
    assert_eq!(mins(o.start, o.end), 420);
    // Autumn: 2026-11-01 02:00 EDT falls back to 01:00 EST, so the night is 9 hours.
    let o = occurrence_on(&ny, date("2026-10-31")).unwrap();
    assert_eq!((o.start, o.end), (utc(2026, 11, 1, 2, 0, 0), utc(2026, 11, 1, 11, 0, 0)));
    assert_eq!(mins(o.start, o.end), 540);
    // Phoenix keeps MST all year: 8 hours both nights.
    for d in ["2026-03-07", "2026-10-31"] {
        let o = occurrence_on(&az, date(d)).unwrap();
        assert_eq!(mins(o.start, o.end), 480, "{d}");
    }
    let o = occurrence_on(&az, date("2026-03-07")).unwrap();
    assert_eq!(o.start, utc(2026, 3, 8, 5, 0, 0));
    // Clocking in mid-shift fixes the real end.
    let p = plan_clock_in(&[ny.clone()], utc(2026, 3, 8, 4, 0, 0)).unwrap();
    assert_eq!((p.end, mins(p.start, p.end)), (utc(2026, 3, 8, 10, 0, 0), 360));
    let p = plan_clock_in(&[ny.clone()], utc(2026, 11, 1, 3, 0, 0)).unwrap();
    assert_eq!(mins(p.start, p.end), 480);
    // A start inside the skipped hour moves to the first real instant (03:00 EDT).
    let gap = single("g", "2026-03-08", hm(2, 30), hm(4, 0), "America/New_York");
    let o = occurrence_on(&gap, date("2026-03-08")).unwrap();
    assert_eq!((o.start, o.end), (utc(2026, 3, 8, 7, 0, 0), utc(2026, 3, 8, 8, 0, 0)));
    // Per-day split at local midnight on both nights.
    let tz = parse_tz("America/New_York").unwrap();
    let spring: Vec<(String, i64)> = day_segments(utc(2026, 3, 8, 3, 0, 0), utc(2026, 3, 8, 10, 0, 0), &tz)
        .into_iter()
        .map(|d| (d.date.to_string(), d.minutes))
        .collect();
    assert_eq!(spring, vec![("2026-03-07".to_string(), 120), ("2026-03-08".to_string(), 300)]);
    let autumn: Vec<(String, i64)> = day_segments(utc(2026, 11, 1, 2, 0, 0), utc(2026, 11, 1, 11, 0, 0), &tz)
        .into_iter()
        .map(|d| (d.date.to_string(), d.minutes))
        .collect();
    assert_eq!(autumn, vec![("2026-10-31".to_string(), 120), ("2026-11-01".to_string(), 420)]);
}

// ================================================================== pay (pure)

/// Mutant: round_up.
#[test]
fn minutes_times_rate_floors_to_the_cent_and_reports_the_leftover() {
    let ps = utc(2026, 11, 9, 0, 0, 0);
    let pe = utc(2026, 11, 16, 0, 0, 0);
    let rates = vec![rate("w1", "2026-01-01", 1999)];
    let p = period_pay(&[seg("2026-11-10", utc(2026, 11, 10, 16, 0, 0), 7)], ps, pe, &rates);
    // 1999 cents/hour x 7 minutes = 233.2166... cents
    assert_eq!((p.minutes, p.cents), (7, 233));
    assert_eq!((p.leftover_units, p.units_per_cent), (130_000, 600_000));
    assert_eq!(p.days[0].exact_cents, "233.216666");
    // the floor is taken once per period, on the exact sum
    let three: Vec<WorkSegment> = (10..13).map(|d| seg(&format!("2026-11-{d}"), utc(2026, 11, d, 16, 0, 0), 7)).collect();
    let p = period_pay(&three, ps, pe, &rates);
    assert_eq!((p.minutes, p.cents, p.leftover_units), (21, 699, 390_000));
    // no rate: nothing paid, reported
    let p = period_pay(&three, ps, pe, &[]);
    assert_eq!((p.cents, p.unpaid_minutes), (0, 21));
    assert!(!p.notes.is_empty());
}

/// Mutant: one_rate_per_period.
#[test]
fn a_rate_change_mid_period_pays_each_day_at_its_own_rate() {
    let rates = vec![rate("w1", "2026-11-01", 1000), rate("w2", "2026-11-11", 2000)];
    assert_eq!(rate_on(&rates, date("2026-11-10")).unwrap().id, "w1");
    assert_eq!(rate_on(&rates, date("2026-11-11")).unwrap().id, "w2");
    assert!(rate_on(&rates, date("2026-10-31")).is_none());
    let p = period_pay(
        &[seg("2026-11-10", utc(2026, 11, 10, 16, 0, 0), 60), seg("2026-11-12", utc(2026, 11, 12, 16, 0, 0), 60)],
        utc(2026, 11, 9, 0, 0, 0),
        utc(2026, 11, 16, 0, 0, 0),
        &rates,
    );
    assert_eq!(p.cents, 1000 + 2000);
    let by_day: Vec<(String, Option<i64>)> = p.days.iter().map(|d| (d.date.clone(), d.rate_cents_per_hour)).collect();
    assert_eq!(by_day, vec![("2026-11-10".into(), Some(1000)), ("2026-11-12".into(), Some(2000))]);
}

/// Mutant: no_overtime.
#[test]
fn overtime_applies_to_minutes_beyond_the_weekly_threshold() {
    let mut r = rate("w1", "2026-11-01", 6000); // 100 cents a minute
    r.overtime_after_minutes = Some(60);
    r.overtime_bps = Some(15_000);
    let rates = vec![r];
    let segs = [seg("2026-11-10", utc(2026, 11, 10, 16, 0, 0), 45), seg("2026-11-11", utc(2026, 11, 11, 16, 0, 0), 45)];
    let p = period_pay(&segs, utc(2026, 11, 9, 0, 0, 0), utc(2026, 11, 16, 0, 0, 0), &rates);
    assert_eq!((p.regular_minutes, p.overtime_minutes), (60, 30));
    assert_eq!(p.cents, 4500 + 1500 + 30 * 150);
    // minutes earlier in the same week, before the period, count toward the threshold
    let p = period_pay(&segs, utc(2026, 11, 11, 0, 0, 0), utc(2026, 11, 16, 0, 0, 0), &rates);
    assert_eq!((p.minutes, p.regular_minutes, p.overtime_minutes, p.cents), (45, 15, 30, 1500 + 4500));
    // a new week starts again (Monday 2026-11-16)
    let p = period_pay(&[seg("2026-11-16", utc(2026, 11, 16, 16, 0, 0), 45)], utc(2026, 11, 16, 0, 0, 0), utc(2026, 11, 23, 0, 0, 0), &rates);
    assert_eq!(p.overtime_minutes, 0);
}

#[test]
fn pay_periods_follow_the_payout_schedule() {
    let p = weekly_policy();
    let slot = timesheet::pay_period(&p.schedule, utc(2026, 11, 12, 9, 0, 0));
    assert_eq!((slot.key.as_str(), slot.start, slot.end), ("weekly-2026-11-09", utc(2026, 11, 9, 0, 0, 0), utc(2026, 11, 16, 0, 0, 0)));
    assert_eq!(timesheet::period_for(&p.schedule, "weekly-2026-11-09").unwrap(), slot);
    assert_eq!(timesheet::period_for(&p.schedule, "2026-11-12").unwrap(), slot);
    assert!(timesheet::period_for(&p.schedule, "weekly-2026-11-10").is_err());
    let manual = PayoutPolicy::from_config(&cfg());
    let m = timesheet::pay_period(&manual.schedule, utc(2026, 11, 12, 9, 0, 0));
    assert_eq!(m.key, "calendar-2026-11");
    let all = timesheet::periods_overlapping(&p.schedule, utc(2026, 11, 15, 23, 0, 0), utc(2026, 11, 16, 1, 0, 0));
    assert_eq!(all.iter().map(|s| s.key.clone()).collect::<Vec<_>>(), vec!["weekly-2026-11-09", "weekly-2026-11-16"]);
}

#[test]
fn a_policy_without_time_auto_approve_reads_and_stores_as_before() {
    let p = weekly_policy();
    assert!(!p.time_auto_approve);
    assert!(!serde_json::to_string(&p).unwrap().contains("time_auto_approve"));
    let mut q = p.clone();
    q.time_auto_approve = true;
    assert!(serde_json::to_string(&q).unwrap().contains("\"time_auto_approve\":true"));
    assert!(q.validate().is_empty());
}

// ================================================================== clock-in (store)

async fn now_shift(pool: &SqlitePool, payee_id: &str, now: DateTime<Utc>) -> shifts::ShiftRow {
    let local = now.with_timezone(&parse_tz("America/Phoenix").unwrap());
    let start = (local - Duration::hours(1)).format("%H:%M").to_string();
    let end = (local + Duration::hours(3)).format("%H:%M").to_string();
    shift(
        pool,
        "manager",
        json!({"payee_id": payee_id, "kind": "single", "date": (local - Duration::hours(1)).format("%Y-%m-%d").to_string(),
               "start_time": start, "end_time": end, "time_zone": "America/Phoenix"}),
    )
    .await
    .unwrap()
}

/// Mutant: second_clock_in_new_entry.
#[tokio::test]
async fn a_second_clock_in_returns_the_same_entry() {
    let pool = pool().await;
    let bob = payee(&pool, "Bob", "bob").await;
    let now = utc(2026, 11, 10, 18, 0, 0);
    now_shift(&pool, &bob, now).await;
    let a = timeclock::clock_in(&pool, &who("bob"), now).await.unwrap();
    assert_eq!(a.result, "created");
    assert_eq!(a.entry.row.payee_id, bob);
    assert_eq!(a.entry.row.end_at, "2026-11-10 21:00:00", "ends at the shift's end, fixed at clock-in");
    let b = timeclock::clock_in(&pool, &who("bob"), now + Duration::minutes(20)).await.unwrap();
    assert_eq!((b.result, b.entry.row.id.as_str()), ("existing", a.entry.row.id.as_str()));
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM time_entries").await, 1);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM audit_log WHERE action = 'time.clock_in'").await, 1);
}

/// Mutants: no_shift_ok, own_payee_by_shift, clock_reads_others.
#[tokio::test]
async fn time_clock_acts_only_on_the_callers_own_payee_and_needs_a_shift() {
    let pool = pool().await;
    let alice = payee(&pool, "Alice", "alice").await;
    let bob = payee(&pool, "Bob", "bob").await;
    let now = utc(2026, 11, 10, 18, 0, 0);
    now_shift(&pool, &bob, now).await;
    // alice has no shift: refused, and bob's shift is never used for her
    let r = timeclock::clock_in(&pool, &who("alice"), now).await;
    assert!(matches!(&r, Err(PayoutError::InvalidState(m)) if m.contains("no shift")), "{r:?}");
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM time_entries").await, 0);
    // someone who is not a payee at all
    assert!(matches!(timeclock::clock_in(&pool, &who("mallory"), now).await, Err(PayoutError::Forbidden(_))));
    // with only time.clock, alice reads her own time and never bob's
    let mut clock_only = who("alice");
    clock_only.scopes = vec![scopes::TIME_CLOCK.into()];
    let mine = timeclock::list(&pool, &clock_only, &EntryFilter::default(), now).await.unwrap();
    assert!(mine.is_empty());
    let theirs = timeclock::list(&pool, &clock_only, &EntryFilter { payee: Some(bob.clone()), ..Default::default() }, now).await;
    assert!(matches!(theirs, Err(PayoutError::Forbidden(_))), "{theirs:?}");
    // control: bob clocks in as bob
    let b = timeclock::clock_in(&pool, &who("bob"), now).await.unwrap();
    assert_eq!(b.entry.row.payee_id, bob);
    let _ = alice;
}

/// Mutant: clock_out_extends.
#[tokio::test]
async fn clock_out_only_ever_shortens_the_entry() {
    let pool = pool().await;
    let bob = payee(&pool, "Bob", "bob").await;
    let now = utc(2026, 11, 10, 18, 0, 0);
    now_shift(&pool, &bob, now).await;
    let a = timeclock::clock_in(&pool, &who("bob"), now).await.unwrap();
    let out = timeclock::clock_out(&pool, &who("bob"), utc(2026, 11, 10, 19, 30, 45)).await.unwrap();
    assert_eq!(out.result, "shortened");
    assert_eq!(out.entry.row.end_at, "2026-11-10 19:30:00", "rounded down to the minute");
    assert!(out.entry.row.end_at <= a.entry.row.end_at);
    assert_eq!(out.entry.minutes, 90);
    // nothing left running: a second clock-out is refused and changes nothing
    assert!(timeclock::clock_out(&pool, &who("bob"), utc(2026, 11, 10, 20, 0, 0)).await.is_err());
    let e: String = sqlx::query_scalar("SELECT end_at FROM time_entries").fetch_one(&pool).await.unwrap();
    assert_eq!(e, "2026-11-10 19:30:00");
    // and clocking in again for the same shift returns the same, shortened entry
    let again = timeclock::clock_in(&pool, &who("bob"), utc(2026, 11, 10, 20, 0, 0)).await.unwrap();
    assert_eq!((again.result, again.entry.row.end_at.as_str()), ("existing", "2026-11-10 19:30:00"));
}

#[tokio::test]
async fn an_open_shift_allowance_lets_a_payee_clock_in_outside_every_shift() {
    let pool = pool().await;
    let cy = payee(&pool, "Cy", "cy").await;
    let now = utc(2026, 11, 10, 18, 0, 0);
    assert!(timeclock::clock_in(&pool, &who("cy"), now).await.is_err(), "no allowance yet");
    shift(&pool, "manager", json!({"payee_id": cy, "kind": "open", "length_minutes": 240, "time_zone": "America/Phoenix"})).await.unwrap();
    let r = timeclock::clock_in(&pool, &who("cy"), now).await.unwrap();
    assert_eq!((r.result, r.entry.minutes), ("created", 240));
    let again = timeclock::clock_in(&pool, &who("cy"), now + Duration::hours(1)).await.unwrap();
    assert_eq!((again.result, again.entry.row.id), ("existing", r.entry.row.id));
}

// ================================================================== authority (store)

/// Mutants: payee_sets_own_wage, payee_approves_own_wage, wage_setter_counts.
#[tokio::test]
async fn a_payee_cannot_set_or_approve_their_own_wage_and_the_setter_cannot_approve() {
    let pool = pool().await;
    store(&pool, &weekly_policy()).await;
    let alice = payee(&pool, "Alice", "alice").await;
    let input = || NewWage { rate_cents_per_hour: 2500, effective_from: date("2026-11-01"), currency: None, overtime_after_minutes: None, overtime_bps: None, memo: None };
    let r = wages::set(&pool, &cfg(), &dir(), &who("alice"), &alice, input(), Utc::now()).await;
    assert!(matches!(r, Err(PayoutError::Forbidden(_))), "{r:?}");
    let w = wages::set(&pool, &cfg(), &dir(), &who("staff"), &alice, input(), Utc::now()).await.unwrap();
    assert!(!w.approved);
    let r = wages::approve(&pool, &cfg(), &dir(), &who("staff"), &alice, &w.row.id, Utc::now()).await;
    assert!(matches!(r, Err(PayoutError::Forbidden(_))), "the setter: {r:?}");
    let r = wages::approve(&pool, &cfg(), &dir(), &who("alice"), &alice, &w.row.id, Utc::now()).await;
    assert!(matches!(r, Err(PayoutError::Forbidden(_))), "the payee: {r:?}");
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM payee_wage_approvals").await, 0);
    let ok = wages::approve(&pool, &cfg(), &dir(), &who("owner1"), &alice, &w.row.id, Utc::now()).await.unwrap();
    assert!(ok.wage.approved);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM audit_log WHERE action IN ('wage.set', 'wage.approved')").await, 2);
    let h = wages::history(&pool, &cfg(), &dir(), &who("staff"), &alice, Utc::now()).await.unwrap();
    assert_eq!(h.in_force.unwrap().row.id, w.row.id);
}

/// Mutant: unapproved_wage_pays.
#[tokio::test]
async fn a_wage_without_approval_pays_nothing() {
    let pool = pool().await;
    store(&pool, &weekly_policy()).await;
    let alice = payee(&pool, "Alice", "alice").await;
    let wid = wage(&pool, &alice, 6000, "2026-11-01", false).await;
    entry(&pool, &alice, utc(2026, 11, 10, 16, 0, 0), utc(2026, 11, 10, 17, 0, 0), "America/Phoenix", "approved").await;
    let p = weekly_policy();
    let slot = timesheet::pay_period(&p.schedule, utc(2026, 11, 10, 16, 0, 0));
    let pay = timesheet::compute(&pool, &dir(), &p, &alice, &slot).await.unwrap();
    assert_eq!((pay.minutes, pay.cents, pay.unpaid_minutes), (60, 0, 60));
    wages::approve(&pool, &cfg(), &dir(), &who("owner1"), &alice, &wid, Utc::now()).await.unwrap();
    let pay = timesheet::compute(&pool, &dir(), &p, &alice, &slot).await.unwrap();
    assert_eq!(pay.cents, 6000, "control: approved, it pays");
}

/// Mutant: payee_creates_own_shift.
#[tokio::test]
async fn a_payee_cannot_create_their_own_shift() {
    let pool = pool().await;
    let alice = payee(&pool, "Alice", "alice").await;
    let body = json!({"payee_id": alice, "kind": "weekly", "weekday": "monday", "start_time": "09:00", "end_time": "17:00", "time_zone": "America/Phoenix"});
    let r = shift(&pool, "alice", body.clone()).await;
    assert!(matches!(r, Err(PayoutError::Forbidden(_))), "{r:?}");
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM shifts").await, 0);
    let s = shift(&pool, "manager", body).await.unwrap();
    let r = shifts::update(&pool, &dir(), &who("alice"), &s.id, serde_json::from_value(json!({"end_time": "23:00"})).unwrap(), Utc::now()).await;
    assert!(matches!(r, Err(PayoutError::Forbidden(_))), "{r:?}");
    // an invalid zone or time is refused
    let bad = shift(&pool, "manager", json!({"payee_id": alice, "kind": "single", "date": "2026-11-10", "start_time": "9am", "end_time": "17:00", "time_zone": "Mars/Olympus"})).await;
    assert!(matches!(bad, Err(PayoutError::Invalid(e)) if e.len() == 2));
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM audit_log WHERE action = 'shift.created'").await, 1);
}

/// Mutants: payee_adjusts_own_entry, payee_voids_own_entry, payee_approves_own_entry.
#[tokio::test]
async fn a_payee_cannot_adjust_void_or_approve_their_own_time() {
    let pool = pool().await;
    let alice = payee(&pool, "Alice", "alice").await;
    let e = entry(&pool, &alice, utc(2026, 11, 10, 16, 0, 0), utc(2026, 11, 10, 18, 0, 0), "America/Phoenix", "open").await;
    let later = utc(2026, 11, 11, 0, 0, 0);
    let adj = AdjustBody { start_at: None, end_at: Some("2026-11-10T20:00:00Z".into()), reason: Some("stayed late".into()) };
    assert!(matches!(timeclock::adjust(&pool, &dir(), &who("alice"), &e, adj.clone(), later).await, Err(PayoutError::Forbidden(_))));
    assert!(matches!(timeclock::void(&pool, &dir(), &who("alice"), &e, Some("x"), later).await, Err(PayoutError::Forbidden(_))));
    assert!(matches!(timeclock::approve(&pool, &dir(), &who("alice"), &e, later).await, Err(PayoutError::Forbidden(_))));
    let row: (String, String) = sqlx::query_as("SELECT status, end_at FROM time_entries").fetch_one(&pool).await.unwrap();
    assert_eq!(row, ("open".to_string(), "2026-11-10 18:00:00".to_string()));
    // control: a manager adjusts (a reason is required), an approver approves
    let mut no_reason = adj.clone();
    no_reason.reason = None;
    assert!(matches!(timeclock::adjust(&pool, &dir(), &who("manager"), &e, no_reason, later).await, Err(PayoutError::Invalid(_))));
    let v = timeclock::adjust(&pool, &dir(), &who("manager"), &e, adj, later).await.unwrap();
    assert_eq!((v.row.end_at.as_str(), v.minutes), ("2026-11-10 20:00:00", 240));
    // an open entry (end not passed) cannot be approved yet
    assert!(timeclock::approve(&pool, &dir(), &who("owner1"), &e, utc(2026, 11, 10, 19, 0, 0)).await.is_err());
    let v = timeclock::approve(&pool, &dir(), &who("owner1"), &e, later).await.unwrap();
    assert_eq!((v.state, v.row.approved_by.as_deref()), ("approved", Some("owner1")));
    // adjusting an approved entry sends it back for approval
    let v = timeclock::adjust(&pool, &dir(), &who("manager"), &e, AdjustBody { start_at: None, end_at: Some("2026-11-10 19:00".into()), reason: Some("left at 12".into()) }, later).await.unwrap();
    assert_eq!(v.state, "closed");
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM audit_log WHERE action IN ('time.adjust', 'time.approve')").await, 3);
}

/// Mutant: chat_unverified_clocks.
#[tokio::test]
async fn an_unverified_chat_identity_cannot_clock_in() {
    use crate::chat::{commands, identity, tests as chat};
    let e = chat::env_with(false).await;
    let pool = e.ctx.pool.clone();
    add_contributor(&pool, "alice", None, false).await;
    let alice = payee(&pool, "Alice", "alice-member").await;
    let now = utc(2026, 11, 10, 18, 0, 0);
    now_shift(&pool, &alice, now).await;
    let mallory = crate::chat::ChatActor::discord("666", Some(false));
    // mallory typed alice's login with `link`: an unverified binding
    assert!(identity::bind(&pool, &mallory, "alice", false, "link").await.unwrap());
    let r = commands::clock_in(&e.ctx, &mallory, now).await;
    assert!(r.private);
    assert!(r.text.contains("not verified"), "{}", r.text);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM time_entries").await, 0);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM audit_log WHERE action = 'time.clock_in.refused'").await, 1);
    // an account never linked at all
    let r = commands::clock_in(&e.ctx, &crate::chat::ChatActor::slack("T1", "U9", None), now).await;
    assert!(r.text.contains("cannot clock in"), "{}", r.text);
    // control: a verified binding clocks in, privately, and a second clock-in is the same entry
    let alice_chat = crate::chat::ChatActor::slack("T1", "U1", None);
    sqlx::query("UPDATE chat_identities SET verified = 1").execute(&pool).await.unwrap();
    let r = commands::clock_in(&e.ctx, &mallory, now).await;
    assert!(r.private && r.text.contains("Clocked in"), "{}", r.text);
    let r = commands::clock_in(&e.ctx, &mallory, now + Duration::minutes(5)).await;
    assert!(r.text.contains("already clocked in"), "{}", r.text);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM time_entries").await, 1);
    let _ = alice_chat;
}

#[tokio::test]
async fn a_cra_listed_member_clocks_in_from_discord_and_slack_and_sees_their_shift() {
    use crate::chat::{commands, tests as chat, ChatActor};
    let e = chat::env_with(true).await;
    let pool = e.ctx.pool.clone();
    let mckale = payee(&pool, "McKale", "mckale").await;
    let now = utc(2026, 11, 10, 18, 0, 0);
    now_shift(&pool, &mckale, now).await;
    let d = ChatActor::discord("100000000000000102", None);
    let s = ChatActor::slack("T1000000000", "U1000000102", None);
    let r = commands::shift(&e.ctx, &d, now).await;
    assert!(r.private && r.text.starts_with("Your current shift"), "{}", r.text);
    let a = commands::clock_in(&e.ctx, &d, now).await;
    assert!(a.private && a.text.contains("Clocked in"), "{}", a.text);
    let b = commands::clock_in(&e.ctx, &s, now).await;
    assert!(b.private && b.text.contains("already clocked in"), "the same person on Slack: {}", b.text);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM time_entries").await, 1);
    let who: String = sqlx::query_scalar("SELECT clocked_in_by FROM time_entries").fetch_one(&pool).await.unwrap();
    assert_eq!(who, "discord:100000000000000102");
    // someone not in the CRA
    let r = commands::clock_in(&e.ctx, &ChatActor::discord("999", None), now).await;
    assert!(r.text.contains("not in the CRA member list"), "{}", r.text);
}

// ================================================================== time to pay (store)

/// Mutants: no_midnight_split, no_period_cut.
#[tokio::test]
async fn an_entry_crossing_midnight_and_a_period_boundary_is_split() {
    let pool = pool().await;
    store(&pool, &weekly_policy()).await;
    let alice = payee(&pool, "Alice", "alice").await;
    wage(&pool, &alice, 6000, "2026-11-01", true).await;
    // Sunday 16:00 to Monday 01:00 in Phoenix (23:00 UTC Sun to 08:00 UTC Mon): crosses the
    // period boundary (Monday 00:00 UTC = Sunday 17:00 local) and local midnight (07:00 UTC).
    entry(&pool, &alice, utc(2026, 11, 15, 23, 0, 0), utc(2026, 11, 16, 8, 0, 0), "America/Phoenix", "approved").await;
    let p = weekly_policy();
    let a = timesheet::period_for(&p.schedule, "weekly-2026-11-09").unwrap();
    let b = timesheet::period_for(&p.schedule, "weekly-2026-11-16").unwrap();
    let pa = timesheet::compute(&pool, &dir(), &p, &alice, &a).await.unwrap();
    let pb = timesheet::compute(&pool, &dir(), &p, &alice, &b).await.unwrap();
    let days = |x: &timesheet::PeriodPay| x.days.iter().map(|d| (d.date.clone(), d.minutes)).collect::<Vec<_>>();
    assert_eq!(days(&pa), vec![("2026-11-15".to_string(), 60)]);
    assert_eq!(days(&pb), vec![("2026-11-15".to_string(), 420), ("2026-11-16".to_string(), 60)]);
    assert_eq!((pa.cents, pb.cents), (6000, 48_000), "every minute paid once, in its own period");
}

/// Mutant: timesheet_twice.
#[tokio::test]
async fn the_period_row_is_written_exactly_once_across_two_scheduler_ticks() {
    let pool = pool().await;
    store(&pool, &weekly_policy()).await;
    let alice = payee(&pool, "Alice", "alice").await;
    wage(&pool, &alice, 3000, "2026-11-01", true).await;
    entry(&pool, &alice, utc(2026, 11, 10, 16, 0, 0), utc(2026, 11, 10, 20, 0, 0), "America/Phoenix", "approved").await;
    let c = ctx(&pool, Arc::new(MockStripe::default()));
    // before the period ends: nothing
    schedule::scheduler_tick(&c, utc(2026, 11, 15, 23, 0, 0)).await.unwrap();
    assert!(timesheet_rows(&pool).await.is_empty());
    let after = utc(2026, 11, 16, 1, 0, 0);
    schedule::scheduler_tick(&c, after).await.unwrap();
    schedule::scheduler_tick(&c, after + Duration::minutes(1)).await.unwrap();
    let rows = timesheet_rows(&pool).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let r = &rows[0];
    assert_eq!((r.source_kind.as_str(), r.amount_cents, r.status.as_str()), (SOURCE_TIMESHEET, 12_000, ledger::PENDING));
    assert_eq!(r.source_ref.as_deref(), Some(format!("{alice}:weekly-2026-11-09").as_str()));
    assert_eq!((r.pr_id, r.merge_sha.as_str()), (-3, format!("timesheet:{alice}:weekly-2026-11-09").as_str()));
    assert_eq!(r.pay_after.as_deref(), Some("2026-11-16 00:00:00"));
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM timesheet_rows").await, 1);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM audit_log WHERE action = 'timesheet.materialized'").await, 1);
    // the ledger refuses a second row with the same source key
    let dup = sqlx::query(
        "INSERT INTO payout_ledger (id, pr_id, pr_number, merge_sha, title, github_username, branch, base_amount_cents, merit_points,
         amount_cents, currency, source_kind, source_ref, payee_id) VALUES ('x', -3, 0, 'timesheet:other', 't', 'alice', 'timesheet', 1, 0, 1, 'usd', 'timesheet', ?, ?)",
    )
    .bind(&r.source_ref)
    .bind(&alice)
    .execute(&pool)
    .await;
    assert!(dup.is_err());
}

/// Mutant: void_pays.
#[tokio::test]
async fn a_voided_entry_pays_nothing() {
    let pool = pool().await;
    store(&pool, &weekly_policy()).await;
    let alice = payee(&pool, "Alice", "alice").await;
    wage(&pool, &alice, 6000, "2026-11-01", true).await;
    entry(&pool, &alice, utc(2026, 11, 10, 16, 0, 0), utc(2026, 11, 10, 17, 0, 0), "America/Phoenix", "approved").await;
    let v = entry(&pool, &alice, utc(2026, 11, 11, 16, 0, 0), utc(2026, 11, 11, 17, 0, 0), "America/Phoenix", "approved").await;
    timeclock::void(&pool, &dir(), &who("manager"), &v, Some("clocked in by mistake"), utc(2026, 11, 12, 0, 0, 0)).await.unwrap();
    let c = ctx(&pool, Arc::new(MockStripe::default()));
    timesheet::materialize_due(&c, utc(2026, 11, 16, 1, 0, 0)).await.unwrap();
    let rows = timesheet_rows(&pool).await;
    assert_eq!(rows.iter().map(|r| r.amount_cents).collect::<Vec<_>>(), vec![6000]);
    // a paid-in entry cannot be voided or adjusted afterwards
    let first: String = sqlx::query_scalar("SELECT id FROM time_entries WHERE status = 'approved'").fetch_one(&pool).await.unwrap();
    assert!(matches!(timeclock::void(&pool, &dir(), &who("manager"), &first, Some("x"), utc(2026, 11, 17, 0, 0, 0)).await, Err(PayoutError::InvalidState(_))));
}

async fn pay_run(pool: &SqlitePool, stripe: &MockStripe, now: DateTime<Utc>) -> runs::ExecuteReport {
    let id = match runs::create_run(pool, &cfg(), &dir(), &Principal::legacy_admin(), false, now).await.unwrap() {
        CreateRunResult::Created { run, .. } => run.run.id,
        CreateRunResult::DryRun(_) => panic!("expected a run"),
    };
    runs::execute_run(pool, stripe, &cfg(), &dir(), &Principal::legacy_admin(), &id, now).await.unwrap()
}

/// Mutant: timesheet_pays_directly.
#[tokio::test]
async fn the_timesheet_row_obeys_approvals_hold_limits_blocks_and_the_kill_switch() {
    let pool = pool().await;
    let mut off = weekly_policy();
    off.enabled = false;
    store(&pool, &off).await;
    let alice = payee(&pool, "Alice", "alice").await;
    wage(&pool, &alice, 3000, "2026-11-01", true).await;
    entry(&pool, &alice, utc(2026, 11, 10, 16, 0, 0), utc(2026, 11, 10, 18, 0, 0), "America/Phoenix", "approved").await;
    let stripe = Arc::new(MockStripe::default());
    let c = ctx(&pool, stripe.clone());
    let when = utc(2026, 11, 16, 1, 0, 0);
    timesheet::materialize_due(&c, when).await.unwrap();
    assert_eq!(stripe.transfers.lock().unwrap().len(), 0, "the kill switch is on: nothing moves");
    let row = timesheet_rows(&pool).await.remove(0);
    assert_eq!(row.amount_cents, 6000);
    let blockers = |p: PayoutPolicy, e: ledger::LedgerEntry| {
        let pool = pool.clone();
        async move {
            let facts = approvals::load_facts(&pool, &dir(), e).await.unwrap();
            approvals::evaluate(&p, &facts, when, &approvals::Usage::default())
        }
    };
    use super::approvals::Blocker;
    assert!(blockers(off.clone(), row.clone()).await.contains(&Blocker::PolicyDisabled));
    let on = weekly_policy();
    assert!(blockers(on.clone(), row.clone()).await.iter().any(|b| matches!(b, Blocker::Approvals { have: 0, .. })), "needs a payout approval");
    // the payee can never approve it
    let mut alice_p = who("alice");
    alice_p.scopes.push(scopes::PAYOUTS_APPROVE.into());
    assert!(matches!(approvals::approve_row(&pool, &dir(), &alice_p, &row.id).await, Err(PayoutError::PayeeCannotApprove)));
    approvals::approve_row(&pool, &dir(), &who("owner1"), &row.id).await.unwrap();
    let mut hold = on.clone();
    hold.limits.hold_days = Some(3);
    assert!(blockers(hold, row.clone()).await.iter().any(|b| matches!(b, Blocker::Hold { .. })));
    let mut lim = on.clone();
    lim.limits.max_per_payout_cents = Some(5999);
    assert!(blockers(lim, row.clone()).await.iter().any(|b| matches!(b, Blocker::AboveMaxPerPayout { .. })));
    approvals::block_payee(&pool, &Principal::legacy_admin(), "alice", "investigation").await.unwrap();
    assert!(blockers(on.clone(), row.clone()).await.contains(&Blocker::PayeeBlocked));
    approvals::unblock_payee(&pool, &Principal::legacy_admin(), "alice").await.unwrap();
    assert_eq!(stripe.transfers.lock().unwrap().len(), 0);
    // control: policy on, approved, nothing in the way: one run pays it once
    store(&pool, &on).await;
    let r = pay_run(&pool, &stripe, when).await;
    assert_eq!(r.paid_cents, 6000);
    let t = stripe.transfers.lock().unwrap().clone();
    assert_eq!((t.len(), t[0].destination.as_str()), (1, "acct_alice"));
    assert!(ledger::approve(&pool, stripe.as_ref(), &row.id, "owner1").await.is_err(), "a paid row is never paid again");
    assert_eq!(stripe.transfers.lock().unwrap().len(), 1);
}

/// Mutant: correction_edits_first.
#[tokio::test]
async fn time_approved_after_the_period_row_makes_a_correction_row_and_never_edits_the_first() {
    let pool = pool().await;
    store(&pool, &weekly_policy()).await;
    let alice = payee(&pool, "Alice", "alice").await;
    wage(&pool, &alice, 6000, "2026-11-01", true).await;
    entry(&pool, &alice, utc(2026, 11, 10, 16, 0, 0), utc(2026, 11, 10, 17, 0, 0), "America/Phoenix", "approved").await;
    let late = entry(&pool, &alice, utc(2026, 11, 12, 16, 0, 0), utc(2026, 11, 12, 16, 30, 0), "America/Phoenix", "open").await;
    let c = ctx(&pool, Arc::new(MockStripe::default()));
    timesheet::materialize_due(&c, utc(2026, 11, 16, 1, 0, 0)).await.unwrap();
    let first = timesheet_rows(&pool).await;
    assert_eq!(first.iter().map(|r| r.amount_cents).collect::<Vec<_>>(), vec![6000]);
    // the late entry is approved after the period's row exists
    timeclock::approve(&pool, &dir(), &who("owner1"), &late, utc(2026, 11, 17, 9, 0, 0)).await.unwrap();
    timesheet::materialize_due(&c, utc(2026, 11, 17, 10, 0, 0)).await.unwrap();
    timesheet::materialize_due(&c, utc(2026, 11, 17, 11, 0, 0)).await.unwrap();
    let rows = timesheet_rows(&pool).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!((rows[0].id.as_str(), rows[0].amount_cents), (first[0].id.as_str(), 6000), "the first row is untouched");
    assert_eq!(rows[1].amount_cents, 3000);
    assert_eq!(rows[1].source_ref.as_deref(), Some(format!("{alice}:weekly-2026-11-09:1").as_str()));
    let seqs: Vec<(i64, i64, i64)> = sqlx::query_as("SELECT seq, owed_cents, amount_cents FROM timesheet_rows ORDER BY seq").fetch_all(&pool).await.unwrap();
    assert_eq!(seqs, vec![(0, 6000, 6000), (1, 9000, 3000)]);
    // the statement shows the timesheet source
    let s = statement::statement(&pool, &Principal::legacy_admin(), &alice, "2026-11-01", "2026-11-30").await.unwrap();
    let t = s.sources.iter().find(|x| x.source_kind == SOURCE_TIMESHEET).unwrap();
    assert_eq!((t.pending_cents, t.rows.len()), (9000, 2));
    // and the timesheet view agrees
    let ts = timesheet::timesheet(&pool, &cfg(), &dir(), &who("staff"), Some(&alice), Some("weekly-2026-11-09"), utc(2026, 11, 17, 12, 0, 0)).await.unwrap();
    assert_eq!((ts.pay.minutes, ts.pay.cents, ts.written_cents, ts.rows.len()), (90, 9000, 9000, 2));
    assert_eq!(ts.rate_cents_per_hour_at_start, Some(6000));
}

#[tokio::test]
async fn time_auto_approve_approves_closed_entries_and_without_it_closed_time_waits() {
    let pool = pool().await;
    store(&pool, &weekly_policy()).await;
    let alice = payee(&pool, "Alice", "alice").await;
    wage(&pool, &alice, 6000, "2026-11-01", true).await;
    entry(&pool, &alice, utc(2026, 11, 10, 16, 0, 0), utc(2026, 11, 10, 17, 0, 0), "America/Phoenix", "open").await;
    let c = ctx(&pool, Arc::new(MockStripe::default()));
    let r = timesheet::materialize_due(&c, utc(2026, 11, 16, 1, 0, 0)).await.unwrap();
    assert!(r.auto_approved.is_empty() && timesheet_rows(&pool).await.is_empty(), "closed but not approved: nothing");
    let mut auto = weekly_policy();
    auto.time_auto_approve = true;
    store(&pool, &auto).await;
    let r = timesheet::materialize_due(&c, utc(2026, 11, 16, 2, 0, 0)).await.unwrap();
    assert_eq!(r.auto_approved.len(), 1);
    assert_eq!(timesheet_rows(&pool).await.len(), 1);
    let by: String = sqlx::query_scalar("SELECT approved_by FROM time_entries").fetch_one(&pool).await.unwrap();
    assert_eq!(by, timeclock::AUTO_APPROVER);
}

// ================================================================== REST

macro_rules! app {
    ($ctx:expr) => {
        atest::init_service(App::new().app_data(web::Data::new($ctx)).configure(api::configure)).await
    };
}

async fn token(pool: &SqlitePool, member: &str, scopes: &[&str]) -> String {
    create_token(
        pool,
        &dir(),
        TokenCreator::Bootstrap,
        NewToken { label: format!("{member} t"), member_id: member.into(), scopes: scopes.iter().map(|s| s.to_string()).collect(), payee_keys: vec![], expires_at: None },
        Utc::now(),
    )
    .await
    .unwrap()
    .token
}

/// Mutant: no_time_scopes.
#[actix_web::test]
async fn every_time_route_refuses_a_token_without_its_scope() {
    let pool = pool().await;
    store(&pool, &weekly_policy()).await;
    let alice = payee(&pool, "Alice", "alice").await;
    let s = shift(&pool, "manager", json!({"payee_id": alice, "kind": "weekly", "weekday": "monday", "start_time": "09:00", "end_time": "17:00", "time_zone": "America/Phoenix"})).await.unwrap();
    let e = entry(&pool, &alice, utc(2026, 11, 10, 16, 0, 0), utc(2026, 11, 10, 17, 0, 0), "America/Phoenix", "open").await;
    let w = wage(&pool, &alice, 2000, "2026-11-01", false).await;
    let app = app!(ctx(&pool, Arc::new(MockStripe::default())));
    let new_shift = json!({"payee_id": alice, "kind": "single", "date": "2026-12-01", "start_time": "09:00", "end_time": "17:00", "time_zone": "America/Phoenix"});
    let routes: Vec<(&str, String, Option<Value>, &str, &[&str])> = vec![
        ("POST", "/api/v1/time/clock-in".into(), None, scopes::TIME_CLOCK, &[]),
        ("POST", "/api/v1/time/clock-out".into(), None, scopes::TIME_CLOCK, &[]),
        ("GET", "/api/v1/time/entries".into(), None, scopes::TIME_READ, &[scopes::TIME_CLOCK]),
        ("POST", format!("/api/v1/time/entries/{e}/approve"), None, scopes::TIME_APPROVE, &[]),
        ("POST", format!("/api/v1/time/entries/{e}/adjust"), Some(json!({"end_at": "2026-11-10T18:00:00Z", "reason": "x"})), scopes::TIME_MANAGE, &[]),
        ("POST", format!("/api/v1/time/entries/{e}/void"), Some(json!({"reason": "x"})), scopes::TIME_MANAGE, &[]),
        ("GET", "/api/v1/time/shifts".into(), None, scopes::TIME_READ, &[scopes::TIME_CLOCK]),
        ("POST", "/api/v1/time/shifts".into(), Some(new_shift), scopes::TIME_MANAGE, &[]),
        ("PATCH", format!("/api/v1/time/shifts/{}", s.id), Some(json!({"early_minutes": 5})), scopes::TIME_MANAGE, &[]),
        ("GET", format!("/api/v1/time/timesheet?payee={alice}"), None, scopes::TIME_READ, &[scopes::TIME_CLOCK]),
        ("GET", format!("/api/v1/payees/{alice}/wage"), None, scopes::PAYEES_READ, &[]),
        ("POST", format!("/api/v1/payees/{alice}/wage"), Some(json!({"rate_cents_per_hour": 100, "effective_from": "2026-11-01"})), scopes::PAYEES_MANAGE, &[]),
        ("POST", format!("/api/v1/payees/{alice}/wage/{w}/approve"), None, scopes::PAYORDERS_APPROVE, &[]),
    ];
    let snapshot = |pool: SqlitePool| async move {
        (
            count(&pool, "SELECT COUNT(*) FROM time_entries WHERE status = 'open' AND end_at = '2026-11-10 17:00:00'").await,
            count(&pool, "SELECT COUNT(*) FROM shifts WHERE early_minutes = 15").await,
            count(&pool, "SELECT COUNT(*) FROM payee_wages").await,
            count(&pool, "SELECT COUNT(*) FROM payee_wage_approvals").await,
            count(&pool, "SELECT COUNT(*) FROM time_entry_events").await,
        )
    };
    let before = snapshot(pool.clone()).await;
    for (method, uri, body, scope, also) in &routes {
        let others: Vec<&str> = scopes::ALL.iter().copied().filter(|s| s != scope && !also.contains(s)).collect();
        let tok = token(&pool, "owner2", &others).await;
        let mut req = match *method {
            "GET" => atest::TestRequest::get(),
            "POST" => atest::TestRequest::post(),
            _ => atest::TestRequest::patch(),
        }
        .uri(uri)
        .insert_header(("Authorization", format!("Bearer {tok}")));
        if let Some(b) = body {
            req = req.set_json(b);
        }
        let resp = atest::call_service(&app, req.to_request()).await;
        assert_eq!(resp.status(), 403, "{method} {uri} without {scope}");
        let v: Value = atest::read_body_json(resp).await;
        assert_eq!(v["message"], format!("missing scope {scope}"), "{method} {uri}");
    }
    assert_eq!(snapshot(pool.clone()).await, before, "nothing changed");
    // control: with every scope each GET succeeds
    let all = token(&pool, "owner2", scopes::ALL).await;
    for (method, uri, _, _, _) in routes.iter().filter(|r| r.0 == "GET") {
        let req = atest::TestRequest::get().uri(uri).insert_header(("Authorization", format!("Bearer {all}")));
        let resp = atest::call_service(&app, req.to_request()).await;
        assert_eq!(resp.status(), 200, "{method} {uri}");
    }
}

#[actix_web::test]
async fn time_bodies_cannot_name_a_payee_or_an_actor() {
    let pool = pool().await;
    store(&pool, &weekly_policy()).await;
    let alice = payee(&pool, "Alice", "alice").await;
    let bob = payee(&pool, "Bob", "bob").await;
    let now = Utc::now();
    now_shift(&pool, &bob, now).await;
    let e = entry(&pool, &alice, utc(2026, 11, 10, 16, 0, 0), utc(2026, 11, 10, 17, 0, 0), "America/Phoenix", "open").await;
    let w = wage(&pool, &alice, 2000, "2026-11-01", false).await;
    let app = app!(ctx(&pool, Arc::new(MockStripe::default())));
    let all = token(&pool, "owner2", scopes::ALL).await;
    let alice_tok = token(&pool, "alice", &[scopes::TIME_CLOCK]).await;
    let cases: Vec<(&str, String, Value, &String)> = vec![
        ("POST", "/api/v1/time/clock-in".into(), json!({"payee_id": bob}), &alice_tok),
        ("POST", "/api/v1/time/clock-in".into(), json!({"member_id": "bob"}), &alice_tok),
        ("POST", "/api/v1/time/clock-out".into(), json!({"payee_id": bob}), &alice_tok),
        ("POST", format!("/api/v1/time/entries/{e}/approve"), json!({"approved_by": "mallory"}), &all),
        ("POST", format!("/api/v1/time/entries/{e}/adjust"), json!({"end_at": "2026-11-10T18:00:00Z", "reason": "x", "actor": "mallory"}), &all),
        ("POST", format!("/api/v1/time/entries/{e}/void"), json!({"reason": "x", "voided_by": "mallory"}), &all),
        ("POST", "/api/v1/time/shifts".into(), json!({"payee_id": alice, "kind": "open", "length_minutes": 60, "time_zone": "UTC", "created_by": "mallory"}), &all),
        ("PATCH", "/api/v1/time/shifts/x".into(), json!({"payee_id": bob}), &all),
        ("POST", format!("/api/v1/payees/{alice}/wage"), json!({"rate_cents_per_hour": 100, "effective_from": "2026-11-01", "set_by": "mallory"}), &all),
        ("POST", format!("/api/v1/payees/{alice}/wage/{w}/approve"), json!({"approver_member_id": "mallory"}), &all),
    ];
    for (method, uri, body, tok) in &cases {
        let req = if *method == "POST" { atest::TestRequest::post() } else { atest::TestRequest::patch() }
            .uri(uri)
            .insert_header(("Authorization", format!("Bearer {tok}")))
            .set_json(body);
        let resp = atest::call_service(&app, req.to_request()).await;
        assert_eq!(resp.status(), 400, "{method} {uri} {body}");
        let v: Value = atest::read_body_json(resp).await;
        assert!(v["message"].as_str().unwrap().contains("unknown field"), "{v}");
    }
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM time_entries").await, 1);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM payee_wages").await, 1);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM payee_wage_approvals").await, 0);
    // control: alice's empty clock-in acts on alice (who has no shift), never on bob
    let req = atest::TestRequest::post().uri("/api/v1/time/clock-in").insert_header(("Authorization", format!("Bearer {alice_tok}")));
    let resp = atest::call_service(&app, req.to_request()).await;
    assert_eq!(resp.status(), 409);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM time_entries").await, 1);
    // and the approver is the token's member
    let req = atest::TestRequest::post().uri(&format!("/api/v1/payees/{alice}/wage/{w}/approve")).insert_header(("Authorization", format!("Bearer {all}")));
    let resp = atest::call_service(&app, req.to_request()).await;
    assert_eq!(resp.status(), 200);
    let v: Value = atest::read_body_json(resp).await;
    assert_eq!(v["result"]["approver_member_id"], "owner2");
}
