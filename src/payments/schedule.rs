//! Payout schedule. The "is a run due, and what is its period key" logic is pure, with the clock
//! passed in; the background task only calls it and lets `UNIQUE (org_id, period_key)` make
//! creation once-per-period even across restarts or concurrent schedulers.

use super::api::ApiContext;
use super::policy::{Cadence, PayoutPolicy, Schedule, ScheduleDay, ScheduleMode, WEEKDAYS};
use super::runs;
use crate::auth::{scopes, Principal};
use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc, Weekday};
use std::time::Duration as StdDuration;
use tracing::{error, info};

/// A schedule period: the slot at which its run becomes due, and the slot that ends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodSlot {
    pub key: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

fn weekday_of(name: &str) -> Option<Weekday> {
    let i = WEEKDAYS.iter().position(|d| *d == name)?;
    Some(match i {
        0 => Weekday::Mon,
        1 => Weekday::Tue,
        2 => Weekday::Wed,
        3 => Weekday::Thu,
        4 => Weekday::Fri,
        5 => Weekday::Sat,
        _ => Weekday::Sun,
    })
}

fn at_hour(d: NaiveDate, hour: u32) -> Option<DateTime<Utc>> {
    Some(Utc.from_utc_datetime(&d.and_hms_opt(hour, 0, 0)?))
}

/// Biweekly slots fall in even weeks counted from Monday 1970-01-05.
fn is_even_week(d: NaiveDate) -> bool {
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 5).expect("valid date");
    (d - epoch).num_days().div_euclid(7) % 2 == 0
}

fn add_months(year: i32, month: u32, delta: i32) -> (i32, u32) {
    let m0 = year * 12 + (month as i32 - 1) + delta;
    (m0.div_euclid(12), (m0.rem_euclid(12) + 1) as u32)
}

/// The latest schedule slot at or before `now`, or None for `manual` cadence or an invalid schedule.
pub fn current_slot(schedule: &Schedule, now: DateTime<Utc>) -> Option<PeriodSlot> {
    let hour = u32::try_from(schedule.hour_utc?).ok().filter(|h| *h <= 23)?;
    match schedule.cadence {
        Cadence::Manual => None,
        Cadence::Weekly | Cadence::Biweekly => {
            let Some(ScheduleDay::Weekday(name)) = &schedule.day else { return None };
            let wd = weekday_of(name)?;
            let biweekly = schedule.cadence == Cadence::Biweekly;
            let span = if biweekly { 14 } else { 7 };
            let today = now.date_naive();
            for back in 0..=span {
                let d = today - Duration::days(back);
                if d.weekday() != wd || (biweekly && !is_even_week(d)) {
                    continue;
                }
                let start = at_hour(d, hour)?;
                if start <= now {
                    let tag = if biweekly { "biweekly" } else { "weekly" };
                    return Some(PeriodSlot {
                        key: format!("{tag}-{}", d.format("%Y-%m-%d")),
                        start,
                        end: start + Duration::days(span),
                    });
                }
            }
            None
        }
        Cadence::Monthly => {
            let Some(ScheduleDay::MonthDay(day)) = &schedule.day else { return None };
            let day = u32::try_from(*day).ok().filter(|d| (1..=28).contains(d))?;
            let this = at_hour(NaiveDate::from_ymd_opt(now.year(), now.month(), day)?, hour)?;
            let (y, m) = if this <= now { (now.year(), now.month()) } else { add_months(now.year(), now.month(), -1) };
            let start = at_hour(NaiveDate::from_ymd_opt(y, m, day)?, hour)?;
            let (ny, nm) = add_months(y, m, 1);
            let end = at_hour(NaiveDate::from_ymd_opt(ny, nm, day)?, hour)?;
            Some(PeriodSlot {
                key: format!("monthly-{y:04}-{m:02}"),
                start,
                end,
            })
        }
    }
}

/// The window used for `max_per_member_per_period_cents`: the current schedule period, or the
/// calendar month (UTC) containing `now` when the cadence is manual.
pub fn period_window(schedule: &Schedule, now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    if let Some(slot) = current_slot(schedule, now) {
        return (slot.start, slot.end);
    }
    let first = NaiveDate::from_ymd_opt(now.year(), now.month(), 1).expect("valid date");
    let (ny, nm) = add_months(now.year(), now.month(), 1);
    let next = NaiveDate::from_ymd_opt(ny, nm, 1).expect("valid date");
    (
        at_hour(first, 0).expect("valid time"),
        at_hour(next, 0).expect("valid time"),
    )
}

/// Is a scheduled run due at `now`? Returns its period key when the policy is enabled, the
/// cadence is not manual, and `last_key` (the newest scheduled run's key) is a different period.
pub fn run_due(policy: &PayoutPolicy, now: DateTime<Utc>, last_key: Option<&str>) -> Option<String> {
    if !policy.enabled {
        return None;
    }
    let slot = current_slot(&policy.schedule, now)?;
    if last_key == Some(slot.key.as_str()) {
        None
    } else {
        Some(slot.key)
    }
}

/// What one scheduler tick did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickOutcome {
    /// Nothing due (disabled, manual cadence, or the period already has its run).
    Idle,
    /// A run was created for the period; `executed` is true in auto mode.
    Created { run_id: String, period_key: String, executed: bool },
}

/// One scheduler step at time `now`. Safe to call any number of times per period.
///
/// First it materializes the due occurrences of active, approved pay orders into ledger rows
/// (contract 11.3; never a transfer) and marks invoices whose ledger row was paid. A failure
/// there is logged and does not stop the payout run below. Then, as before, it creates the
/// period's scheduled run when one is due.
pub async fn scheduler_tick(ctx: &ApiContext, now: DateTime<Utc>) -> Result<TickOutcome, super::error::PayoutError> {
    if let Err(e) = super::payorders::materialize_due(ctx, now).await {
        error!("pay order materialization failed: {}", e);
    }
    if let Err(e) = super::invoices::sync_paid(&ctx.pool, now).await {
        error!("invoice paid sync failed: {}", e);
    }
    let org = crate::auth::DEFAULT_ORG;
    let eff = super::policy::load_effective(&ctx.pool, org, &ctx.payments).await?;
    let last: Option<String> = sqlx::query_scalar(
        "SELECT period_key FROM payout_runs WHERE org_id = ? AND period_key IS NOT NULL ORDER BY created_at DESC, rowid DESC LIMIT 1",
    )
    .bind(org)
    .fetch_optional(&ctx.pool)
    .await?;
    let Some(key) = run_due(&eff.policy, now, last.as_deref()) else {
        return Ok(TickOutcome::Idle);
    };
    let system = Principal::system("scheduler", &[scopes::PAYOUTS_PREPARE, scopes::PAYOUTS_EXECUTE, scopes::PAYOUTS_READ]);
    let Some(run_id) = runs::create_scheduled_run(&ctx.pool, &ctx.payments, ctx.directory.as_ref(), &system, &key, now).await? else {
        return Ok(TickOutcome::Idle);
    };
    let mut executed = false;
    if eff.policy.schedule.mode == ScheduleMode::Auto {
        runs::execute_run(&ctx.pool, ctx.stripe.as_ref(), &ctx.payments, ctx.directory.as_ref(), &system, &run_id, now).await?;
        executed = true;
        if let Err(e) = super::invoices::sync_paid(&ctx.pool, now).await {
            error!("invoice paid sync failed: {}", e);
        }
    }
    info!("Scheduled payout run {} created for {} (executed: {})", run_id, key, executed);
    Ok(TickOutcome::Created { run_id, period_key: key, executed })
}

/// Background task: tick every `interval`. Started from main.rs.
pub async fn run_scheduler(ctx: ApiContext, interval: StdDuration) {
    info!("Payout scheduler started (interval {:?})", interval);
    loop {
        if let Err(e) = scheduler_tick(&ctx, Utc::now()).await {
            error!("payout scheduler tick failed: {}", e);
        }
        tokio::time::sleep(interval).await;
    }
}
