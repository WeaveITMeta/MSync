//! Pay orders (contract 11.3 and 11.5): an instruction to pay, with an amount rule and a schedule.
//!
//! Layout:
//! - Money arithmetic is pure ([`split_fixed`], [`split_pool_percent`], [`split_pool_by_xp`],
//!   [`split_percent_of_amount`]): integer cents, basis points, floor per payee, leftover
//!   reported and never given to anyone.
//! - "Which occurrences are due, and their period keys" is pure ([`occurrence`],
//!   [`due_occurrences`]) with the clock passed in.
//! - [`materialize_due`] writes each due occurrence exactly once (`UNIQUE (order_id,
//!   period_key)`) into one ledger row per payee. It never transfers; payout runs pay the rows.
//!
//! Authority: the creator and the author of the current revision never approve it; an order
//! materializes nothing until it has the policy's required approvals from distinct members at or
//! above `min_approver_rank`; a change to the amount rule, payees, schedule or currency starts a
//! new revision, which clears the approvals; a payee of an order can never create, edit, approve,
//! pause, resume or cancel it.

use super::api::ApiContext;
use super::config::PaymentConfig;
use super::error::PayoutError;
use super::ledger::{self, SourceRow, SOURCE_PAY_ORDER};
use super::payees::{self, double_option, Payee};
use super::policy::{self, PayoutPolicy};
use crate::auth::{
    is_payee, member_payee_keys, scopes, tokens::format_db_time, tokens::parse_db_time, Directory,
    Principal,
};
use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{Row, SqliteConnection, SqlitePool};
use std::collections::{BTreeMap, HashMap, HashSet};
use tracing::{error, info, warn};
use uuid::Uuid;

/// 100% in basis points.
pub const BPS_FULL: i64 = 10_000;
/// Largest amount any rule accepts, in cents (ten billion dollars). Keeps every product in range.
pub const MAX_CENTS: i64 = 1_000_000_000_000;
/// Most occurrences a repeat schedule may have, and the most preview periods.
pub const MAX_OCCURRENCES: u32 = 1000;
pub const MAX_PREVIEW_PERIODS: u32 = 24;

pub const DRAFT: &str = "draft";
pub const ACTIVE: &str = "active";
pub const PAUSED: &str = "paused";
pub const ENDED: &str = "ended";
pub const CANCELLED: &str = "cancelled";

// ================================================================== amount rules

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AmountRule {
    Fixed(FixedRule),
    PoolPercent(PoolPercentRule),
    PoolByXp(PoolByXpRule),
    PercentOfAmount(PercentOfAmountRule),
}

/// A set amount to one payee (salary, stipend, retainer, bonus).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixedRule {
    pub payee_id: String,
    pub amount_cents: i64,
}

/// A pool per period, split by fixed percentages. Sum of bps at most 10000; the rest is not paid.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolPercentRule {
    pub pool_cents: i64,
    pub shares: Vec<PercentShare>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PercentShare {
    pub payee_id: String,
    pub bps: i64,
}

/// A pool per period, split in proportion to XP earned in the period.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolByXpRule {
    pub pool_cents: i64,
    /// Only XP earned in these repositories counts. Empty = every repository (and XP with none).
    #[serde(default)]
    pub repos: Vec<RepoFilter>,
    /// Only people holding one of these roles (merit role or CRA role key). Empty = anyone.
    #[serde(default)]
    pub roles: Vec<String>,
    /// Only these payees. Empty = anyone with qualifying XP.
    #[serde(default)]
    pub payee_ids: Vec<String>,
    /// XP in the period needed to qualify (default 1).
    #[serde(default)]
    pub min_xp: i64,
    /// Most one payee can receive, in bps of the pool. The excess is not paid.
    #[serde(default)]
    pub max_share_bps: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoFilter {
    /// github | gitlab
    pub provider: String,
    /// Defaults to github.com or gitlab.com.
    #[serde(default)]
    pub host: Option<String>,
    pub repo: String,
}

/// A percentage of a stated base amount to one payee. The base is `base_cents` for every
/// occurrence, or `bases[period_key]` for one occurrence (which wins).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PercentOfAmountRule {
    pub payee_id: String,
    pub bps: i64,
    #[serde(default)]
    pub base_cents: Option<i64>,
    #[serde(default)]
    pub bases: BTreeMap<String, i64>,
}

fn amount_ok(errs: &mut Vec<String>, field: &str, v: i64) {
    if !(0..=MAX_CENTS).contains(&v) {
        errs.push(format!("{field} must be between 0 and {MAX_CENTS} cents"));
    }
}

fn bps_ok(errs: &mut Vec<String>, field: &str, v: i64) {
    if !(0..=BPS_FULL).contains(&v) {
        errs.push(format!(
            "{field} must be between 0 and {BPS_FULL} basis points (100%)"
        ));
    }
}

impl AmountRule {
    /// The payees this rule can pay. None = open to any payee (a `pool_by_xp` with no payee list).
    pub fn payee_ids(&self) -> Option<Vec<String>> {
        match self {
            AmountRule::Fixed(r) => Some(vec![r.payee_id.clone()]),
            AmountRule::PoolPercent(r) => {
                Some(r.shares.iter().map(|s| s.payee_id.clone()).collect())
            }
            AmountRule::PoolByXp(r) if r.payee_ids.is_empty() => None,
            AmountRule::PoolByXp(r) => Some(r.payee_ids.clone()),
            AmountRule::PercentOfAmount(r) => Some(vec![r.payee_id.clone()]),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            AmountRule::Fixed(_) => "fixed",
            AmountRule::PoolPercent(_) => "pool_percent",
            AmountRule::PoolByXp(_) => "pool_by_xp",
            AmountRule::PercentOfAmount(_) => "percent_of_amount",
        }
    }

    /// Shape checks that need no database. Empty = valid.
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        match self {
            AmountRule::Fixed(r) => {
                amount_ok(&mut errs, "amount_cents", r.amount_cents);
                if r.amount_cents == 0 {
                    errs.push("amount_cents must be above 0".into());
                }
            }
            AmountRule::PoolPercent(r) => {
                amount_ok(&mut errs, "pool_cents", r.pool_cents);
                if r.shares.is_empty() {
                    errs.push("shares must name at least one payee".into());
                }
                let mut seen = HashSet::new();
                let mut sum: i64 = 0;
                for s in &r.shares {
                    bps_ok(&mut errs, "bps", s.bps);
                    if !seen.insert(s.payee_id.clone()) {
                        errs.push(format!("payee {} appears twice in shares", s.payee_id));
                    }
                    sum = sum.saturating_add(s.bps);
                }
                if sum > BPS_FULL {
                    errs.push(format!(
                        "shares sum to {sum} basis points; the most is {BPS_FULL} (100%)"
                    ));
                }
            }
            AmountRule::PoolByXp(r) => {
                amount_ok(&mut errs, "pool_cents", r.pool_cents);
                if r.min_xp < 0 {
                    errs.push("min_xp must be 0 or more".into());
                }
                if let Some(m) = r.max_share_bps {
                    bps_ok(&mut errs, "max_share_bps", m);
                }
                for f in &r.repos {
                    if f.provider != "github" && f.provider != "gitlab" {
                        errs.push("repos[].provider must be github or gitlab".into());
                    }
                    if f.repo.trim().is_empty() {
                        errs.push("repos[].repo is required".into());
                    }
                }
                let unique: HashSet<&String> = r.payee_ids.iter().collect();
                if unique.len() != r.payee_ids.len() {
                    errs.push("payee_ids lists a payee twice".into());
                }
            }
            AmountRule::PercentOfAmount(r) => {
                bps_ok(&mut errs, "bps", r.bps);
                if let Some(b) = r.base_cents {
                    amount_ok(&mut errs, "base_cents", b);
                }
                for (k, b) in &r.bases {
                    amount_ok(&mut errs, "bases[]", *b);
                    if NaiveDate::parse_from_str(k, "%Y-%m-%d").is_err() {
                        errs.push(format!(
                            "bases key '{k}' must be an occurrence date (YYYY-MM-DD)"
                        ));
                    }
                }
            }
        }
        errs
    }
}

// ================================================================== money arithmetic (pure)

/// One line of a split. `payee_id` is None for XP earned by someone with no payee record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Allocation {
    pub payee_id: Option<String>,
    /// Who the line is for (payee id, or `login:<payee key>` when there is no payee).
    pub label: String,
    /// Cents written to the ledger for this payee.
    pub cents: i64,
    /// Cents this line earned but that are not paid (cap, no payee, payee not active).
    pub unpaid_cents: i64,
    pub xp: Option<i64>,
    pub note: Option<String>,
}

/// The result of applying an amount rule to one occurrence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Split {
    pub pool_cents: i64,
    pub allocations: Vec<Allocation>,
    /// Sum of `allocations[].cents`. Never above `pool_cents`.
    pub paid_cents: i64,
    /// `pool_cents - paid_cents`: not paid, never given to an arbitrary payee.
    pub leftover_cents: i64,
    /// Why nothing (or less than the pool) is paid, when that is the whole story.
    pub note: Option<String>,
}

impl Split {
    fn finish(pool_cents: i64, allocations: Vec<Allocation>, note: Option<String>) -> Split {
        let paid: i64 = allocations.iter().map(|a| a.cents).sum();
        Split {
            pool_cents,
            allocations,
            paid_cents: paid,
            leftover_cents: pool_cents - paid,
            note,
        }
    }

    /// Move a payee's cents to unpaid (payee paused or closed at materialization).
    pub fn withhold(&mut self, payee_id: &str, why: &str) {
        for a in &mut self.allocations {
            if a.payee_id.as_deref() == Some(payee_id) && a.cents > 0 {
                a.unpaid_cents += a.cents;
                a.cents = 0;
                a.note = Some(why.to_string());
            }
        }
        self.paid_cents = self.allocations.iter().map(|a| a.cents).sum();
        self.leftover_cents = self.pool_cents - self.paid_cents;
    }
}

/// floor(amount * bps / 10000), exact in i128. Errors outside the accepted ranges.
pub fn percent_of(amount_cents: i64, bps: i64) -> Result<i64, String> {
    if !(0..=MAX_CENTS).contains(&amount_cents) {
        return Err(format!("amount must be between 0 and {MAX_CENTS} cents"));
    }
    if !(0..=BPS_FULL).contains(&bps) {
        return Err(format!(
            "percentage must be between 0 and {BPS_FULL} basis points"
        ));
    }
    Ok(((amount_cents as i128 * bps as i128) / BPS_FULL as i128) as i64)
}

fn line(payee_id: &str, cents: i64) -> Allocation {
    Allocation {
        payee_id: Some(payee_id.to_string()),
        label: payee_id.to_string(),
        cents,
        unpaid_cents: 0,
        xp: None,
        note: None,
    }
}

/// `fixed`: the amount to one payee.
pub fn split_fixed(payee_id: &str, amount_cents: i64) -> Result<Split, String> {
    if !(1..=MAX_CENTS).contains(&amount_cents) {
        return Err(format!("amount must be between 1 and {MAX_CENTS} cents"));
    }
    Ok(Split::finish(
        amount_cents,
        vec![line(payee_id, amount_cents)],
        None,
    ))
}

/// `pool_percent`: each payee gets floor(pool * bps / 10000). The percentages must sum to at
/// most 100%; the rounding remainder and any unassigned percentage are the leftover.
pub fn split_pool_percent(pool_cents: i64, shares: &[(String, i64)]) -> Result<Split, String> {
    let mut sum: i64 = 0;
    let mut seen = HashSet::new();
    for (p, bps) in shares {
        if !(0..=BPS_FULL).contains(bps) {
            return Err(format!(
                "percentage must be between 0 and {BPS_FULL} basis points"
            ));
        }
        if !seen.insert(p.clone()) {
            return Err(format!("payee {p} appears twice"));
        }
        sum += bps;
    }
    if sum > BPS_FULL {
        return Err(format!(
            "percentages sum to {sum} basis points; the most is {BPS_FULL}"
        ));
    }
    let mut out = Vec::new();
    for (p, bps) in shares {
        out.push(line(p, percent_of(pool_cents, *bps)?));
    }
    Ok(Split::finish(pool_cents, out, None))
}

/// `percent_of_amount`: floor(base * bps / 10000) to one payee.
pub fn split_percent_of_amount(payee_id: &str, base_cents: i64, bps: i64) -> Result<Split, String> {
    let amount = percent_of(base_cents, bps)?;
    // The "pool" of this rule is the amount owed; there is no leftover by design.
    Ok(Split::finish(amount, vec![line(payee_id, amount)], None))
}

/// XP one person earned in a period, and the payee record they are paid through (if any).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XpInput {
    /// The person's payee key (ledger github_username, lowercased).
    pub key: String,
    pub payee_id: Option<String>,
    pub xp: i64,
}

/// `pool_by_xp`: everyone with at least `min_xp` (and more than 0) XP shares the pool in
/// proportion to XP, each floor(pool * xp / total). XP of several keys of one payee is added up.
/// A share above `max_share_bps` of the pool is cut to it; a share of a person with no payee
/// record is not paid. Every cut, unpaid share and rounding remainder is leftover. Zero qualifying
/// XP pays nothing and says why.
pub fn split_pool_by_xp(
    pool_cents: i64,
    entries: &[XpInput],
    min_xp: i64,
    max_share_bps: Option<i64>,
) -> Result<Split, String> {
    if !(0..=MAX_CENTS).contains(&pool_cents) {
        return Err(format!("pool must be between 0 and {MAX_CENTS} cents"));
    }
    if let Some(m) = max_share_bps {
        if !(0..=BPS_FULL).contains(&m) {
            return Err(format!("max_share_bps must be between 0 and {BPS_FULL}"));
        }
    }
    // Group by payee (or by key when there is none), deterministic order.
    let mut groups: BTreeMap<String, (Option<String>, i64)> = BTreeMap::new();
    for e in entries {
        let label = match &e.payee_id {
            Some(p) => p.clone(),
            None => format!("login:{}", e.key),
        };
        let g = groups.entry(label).or_insert((e.payee_id.clone(), 0));
        g.1 = g.1.saturating_add(e.xp);
    }
    let threshold = min_xp.max(1);
    let qualifying: Vec<(String, Option<String>, i64)> = groups
        .into_iter()
        .filter(|(_, (_, xp))| *xp >= threshold)
        .map(|(l, (p, xp))| (l, p, xp))
        .collect();
    let total: i128 = qualifying.iter().map(|(_, _, xp)| *xp as i128).sum();
    if total <= 0 {
        return Ok(Split::finish(
            pool_cents,
            Vec::new(),
            Some(format!(
                "no one earned {threshold} or more XP in the period; nothing is paid"
            )),
        ));
    }
    let cap = match max_share_bps {
        Some(m) => percent_of(pool_cents, m)?,
        None => pool_cents,
    };
    let mut out = Vec::new();
    for (label, payee_id, xp) in qualifying {
        let share = ((pool_cents as i128 * xp as i128) / total) as i64;
        let capped = share.min(cap);
        let mut a = Allocation {
            payee_id: payee_id.clone(),
            label,
            cents: capped,
            unpaid_cents: share - capped,
            xp: Some(xp),
            note: None,
        };
        if share > capped {
            a.note = Some(format!("share capped at {} cents (max_share_bps)", cap));
        }
        if payee_id.is_none() {
            a.unpaid_cents += a.cents;
            a.cents = 0;
            a.note = Some("no payee record for this person; share not paid".into());
        }
        out.push(a);
    }
    Ok(Split::finish(pool_cents, out, None))
}

// ================================================================== schedules (pure)

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OrderSchedule {
    Once(OnceSchedule),
    Repeat(RepeatSchedule),
}

/// One occurrence on a date (00:00 UTC).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnceSchedule {
    pub date: NaiveDate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepeatCadence {
    Weekly,
    Biweekly,
    Monthly,
    Quarterly,
}

/// Occurrences on `start` and every cadence step after it (00:00 UTC). Monthly and quarterly
/// keep the start's day of month, or the month's last day when it is shorter. `end` (inclusive)
/// and `count` both stop the schedule; the first one reached wins.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepeatSchedule {
    pub cadence: RepeatCadence,
    pub start: NaiveDate,
    #[serde(default)]
    pub end: Option<NaiveDate>,
    #[serde(default)]
    pub count: Option<u32>,
}

/// One schedule occurrence. `period_key` is its date; the XP period is [period_start, period_end).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Occurrence {
    pub index: u32,
    pub period_key: String,
    pub occurs_at: DateTime<Utc>,
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
}

fn midnight(d: NaiveDate) -> DateTime<Utc> {
    Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).expect("valid time"))
}

fn days_in_month(y: i32, m: u32) -> u32 {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    (NaiveDate::from_ymd_opt(ny, nm, 1).expect("valid")
        - NaiveDate::from_ymd_opt(y, m, 1).expect("valid"))
    .num_days() as u32
}

/// `d` moved by `months` (may be negative), keeping its day or clamping to the month's end.
fn shift_months(d: NaiveDate, months: i64) -> Option<NaiveDate> {
    let m0 = d.year() as i64 * 12 + (d.month() as i64 - 1) + months;
    let y = i32::try_from(m0.div_euclid(12)).ok()?;
    let m = (m0.rem_euclid(12) + 1) as u32;
    NaiveDate::from_ymd_opt(y, m, d.day().min(days_in_month(y, m)))
}

impl OrderSchedule {
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        let in_range = |d: &NaiveDate| (2000..=2100).contains(&d.year());
        match self {
            OrderSchedule::Once(o) => {
                if !in_range(&o.date) {
                    errs.push("date must be between 2000 and 2100".into());
                }
            }
            OrderSchedule::Repeat(r) => {
                if !in_range(&r.start) {
                    errs.push("start must be between 2000 and 2100".into());
                }
                if let Some(e) = r.end {
                    if e < r.start {
                        errs.push("end must be on or after start".into());
                    }
                    if !in_range(&e) {
                        errs.push("end must be between 2000 and 2100".into());
                    }
                }
                if let Some(c) = r.count {
                    if c == 0 || c > MAX_OCCURRENCES {
                        errs.push(format!("count must be between 1 and {MAX_OCCURRENCES}"));
                    }
                }
            }
        }
        errs
    }

    /// The date of occurrence `n` (0-based), or None when the schedule has ended before it.
    pub fn nth_date(&self, n: u32) -> Option<NaiveDate> {
        match self {
            OrderSchedule::Once(o) => (n == 0).then_some(o.date),
            OrderSchedule::Repeat(r) => {
                if n >= r.count.unwrap_or(MAX_OCCURRENCES).min(MAX_OCCURRENCES) {
                    return None;
                }
                let d = match r.cadence {
                    RepeatCadence::Weekly => {
                        r.start.checked_add_signed(Duration::days(7 * n as i64))?
                    }
                    RepeatCadence::Biweekly => {
                        r.start.checked_add_signed(Duration::days(14 * n as i64))?
                    }
                    RepeatCadence::Monthly => shift_months(r.start, n as i64)?,
                    RepeatCadence::Quarterly => shift_months(r.start, 3 * n as i64)?,
                };
                match r.end {
                    Some(e) if d > e => None,
                    _ => Some(d),
                }
            }
        }
    }

    /// The date one cadence step before `d` (once: one month).
    fn step_back(&self, d: NaiveDate) -> NaiveDate {
        let back = match self {
            OrderSchedule::Once(_) => shift_months(d, -1),
            OrderSchedule::Repeat(r) => match r.cadence {
                RepeatCadence::Weekly => d.checked_sub_signed(Duration::days(7)),
                RepeatCadence::Biweekly => d.checked_sub_signed(Duration::days(14)),
                RepeatCadence::Monthly => shift_months(d, -1),
                RepeatCadence::Quarterly => shift_months(d, -3),
            },
        };
        back.unwrap_or(d)
    }
}

/// Occurrence `n` of the schedule. Its XP period runs from the previous occurrence (or one step
/// before the first) up to its own date.
pub fn occurrence(s: &OrderSchedule, n: u32) -> Option<Occurrence> {
    let d = s.nth_date(n)?;
    let prev = if n == 0 {
        s.step_back(d)
    } else {
        s.nth_date(n - 1).unwrap_or_else(|| s.step_back(d))
    };
    Some(Occurrence {
        index: n,
        period_key: d.format("%Y-%m-%d").to_string(),
        occurs_at: midnight(d),
        period_start: midnight(prev),
        period_end: midnight(d),
    })
}

/// What a tick should do with one order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DuePlan {
    /// Occurrences at or before `now`, not yet recorded, on or after `resume_from`: materialize.
    pub due: Vec<Occurrence>,
    /// Occurrences at or before `now`, not yet recorded, dated before `resume_from`: they fell
    /// while the order was paused. Recorded as skipped; never paid.
    pub skipped: Vec<Occurrence>,
    /// No occurrence after `now` remains.
    pub schedule_exhausted: bool,
}

/// Pure: which occurrences are due at `now`. `recorded` holds the period keys already written
/// (materialized or skipped); those are never returned again.
pub fn due_occurrences(
    s: &OrderSchedule,
    resume_from: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    recorded: &HashSet<String>,
) -> DuePlan {
    let mut plan = DuePlan::default();
    let mut n = 0u32;
    loop {
        let Some(o) = occurrence(s, n) else {
            plan.schedule_exhausted = true;
            break;
        };
        if o.occurs_at > now {
            break;
        }
        if !recorded.contains(&o.period_key) {
            match resume_from {
                Some(r) if o.occurs_at < r => plan.skipped.push(o),
                _ => plan.due.push(o),
            }
        }
        n += 1;
        if n > MAX_OCCURRENCES {
            plan.schedule_exhausted = true;
            break;
        }
    }
    plan
}

/// The next `count` occurrences not yet recorded and not skipped by a pause, in date order
/// (overdue ones first). Pure.
pub fn upcoming(
    s: &OrderSchedule,
    resume_from: Option<DateTime<Utc>>,
    recorded: &HashSet<String>,
    count: u32,
) -> Vec<Occurrence> {
    let mut out = Vec::new();
    let mut n = 0u32;
    while out.len() < count as usize && n <= MAX_OCCURRENCES {
        let Some(o) = occurrence(s, n) else { break };
        n += 1;
        if recorded.contains(&o.period_key) || resume_from.is_some_and(|r| o.occurs_at < r) {
            continue;
        }
        out.push(o);
    }
    out
}

// ================================================================== storage

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct OrderRow {
    pub id: String,
    pub org_id: String,
    pub name: String,
    pub status: String,
    pub currency: String,
    pub amount_rule: String,
    pub schedule: String,
    pub memo: Option<String>,
    pub revision: i64,
    pub revised_by: String,
    pub resume_from: Option<String>,
    pub created_by: String,
    pub created_at: String,
    pub updated_by: String,
    pub updated_at: String,
}

impl OrderRow {
    fn rule(&self) -> Result<AmountRule, PayoutError> {
        serde_json::from_str(&self.amount_rule).map_err(|e| {
            PayoutError::InvalidState(format!("stored amount rule is unreadable: {e}"))
        })
    }
    fn sched(&self) -> Result<OrderSchedule, PayoutError> {
        serde_json::from_str(&self.schedule)
            .map_err(|e| PayoutError::InvalidState(format!("stored schedule is unreadable: {e}")))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OrderApproval {
    pub approver_member_id: String,
    pub created_at: String,
    /// Does this approval count toward approvals_required right now?
    pub counts: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct OccurrenceRow {
    pub id: String,
    pub period_key: String,
    pub occurs_at: String,
    pub period_start: String,
    pub period_end: String,
    pub status: String,
    pub order_revision: i64,
    pub pool_cents: i64,
    pub paid_cents: i64,
    pub leftover_cents: i64,
    pub detail: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct OrderView {
    pub id: String,
    pub name: String,
    pub status: String,
    pub currency: String,
    pub rule: Value,
    pub schedule: Value,
    pub memo: Option<String>,
    pub revision: i64,
    pub revised_by: String,
    pub resume_from: Option<String>,
    pub created_by: String,
    pub created_at: String,
    pub updated_by: String,
    pub updated_at: String,
    pub approvals: Vec<OrderApproval>,
    pub approvals_counted: i64,
    pub approvals_required: i64,
    /// The current revision has enough approvals to materialize.
    pub approved: bool,
    pub occurrences: Vec<OccurrenceRow>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewOrder {
    pub name: String,
    /// Defaults to the payout policy's currency; must equal it.
    #[serde(default)]
    pub currency: Option<String>,
    pub rule: AmountRule,
    pub schedule: OrderSchedule,
    #[serde(default)]
    pub memo: Option<String>,
    /// true = created as a draft (never materializes until resumed).
    #[serde(default)]
    pub draft: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderPatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, with = "double_option")]
    pub memo: Option<Option<String>>,
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub rule: Option<AmountRule>,
    #[serde(default)]
    pub schedule: Option<OrderSchedule>,
}

async fn load_row(pool: &SqlitePool, id: &str) -> Result<Option<OrderRow>, sqlx::Error> {
    sqlx::query_as::<_, OrderRow>("SELECT * FROM pay_orders WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
}

async fn load_scoped(
    pool: &SqlitePool,
    principal: &Principal,
    id: &str,
) -> Result<OrderRow, PayoutError> {
    let row = load_row(pool, id)
        .await?
        .ok_or(PayoutError::NotFound("pay order"))?;
    if row.org_id != principal.org_id {
        return Err(PayoutError::NotFound("pay order"));
    }
    Ok(row)
}

/// Record a pay order event and its audit entry (`payorder.<action>`) inside the caller's
/// transaction: no audit entry, no change.
#[allow(clippy::too_many_arguments)]
async fn event(
    conn: &mut SqliteConnection,
    order: &str,
    action: &str,
    from: Option<&str>,
    to: Option<&str>,
    revision: i64,
    actor: &str,
    now: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO pay_order_events (order_id, action, from_status, to_status, revision, actor, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(order)
    .bind(action)
    .bind(from)
    .bind(to)
    .bind(revision)
    .bind(actor)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    let verb = match action {
        "create" => "created",
        "update" => "updated",
        "approve" => "approved",
        "pause" => "paused",
        "resume" => "resumed",
        "cancel" => "cancelled",
        "end" => "ended",
        other => other,
    };
    crate::audit::append_in(
        conn,
        &crate::audit::AuditEntry::new(
            crate::audit::kind_for_member(actor),
            actor,
            format!("payorder.{verb}"),
            format!("payorder:{order}"),
            serde_json::json!({ "order_id": order, "from_status": from, "to_status": to, "revision": revision }),
        ),
    )
    .await?;
    Ok(())
}

// ------------------------------------------------------------------ who is a payee of an order

/// Is `member_id` (with extra token payee keys) paid by `payee_ids`? None = an order open to any
/// payee: true when the member owns any payee record or has declared any payee key.
pub async fn member_is_payee_of(
    pool: &SqlitePool,
    directory: &dyn Directory,
    member_id: &str,
    token_keys: &[String],
    payee_ids: Option<&[String]>,
) -> Result<bool, sqlx::Error> {
    let Some(ids) = payee_ids else {
        if !token_keys.is_empty() || !member_payee_keys(pool, member_id).await?.is_empty() {
            return Ok(true);
        }
        let own: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM payees WHERE member_id = ? LIMIT 1")
                .bind(member_id)
                .fetch_optional(pool)
                .await?;
        return Ok(own.is_some());
    };
    for id in ids {
        let Some(p) = payees::load(pool, id).await? else {
            continue;
        };
        if p.row.member_id.as_deref() == Some(member_id) {
            return Ok(true);
        }
        for k in &p.payee_keys {
            if token_keys.iter().any(|t| t.to_lowercase() == *k)
                || is_payee(pool, directory, member_id, k).await?
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

async fn refuse_payee(
    pool: &SqlitePool,
    directory: &dyn Directory,
    principal: &Principal,
    rule: &AmountRule,
    verb: &str,
) -> Result<(), PayoutError> {
    let ids = rule.payee_ids();
    if member_is_payee_of(
        pool,
        directory,
        &principal.member_id,
        &principal.payee_keys,
        ids.as_deref(),
    )
    .await?
    {
        return Err(PayoutError::Forbidden(format!(
            "a payee cannot {verb} a pay order that pays them"
        )));
    }
    Ok(())
}

// ------------------------------------------------------------------ approvals

/// Count the approvals of the order's current revision that satisfy the policy: distinct, not
/// the creator, not the author of the revision, not a payee of the order, rank >= min_approver_rank.
pub async fn order_approvals(
    pool: &SqlitePool,
    directory: &dyn Directory,
    policy: &PayoutPolicy,
    row: &OrderRow,
    rule: &AmountRule,
) -> Result<(Vec<OrderApproval>, i64), sqlx::Error> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT approver_member_id, created_at FROM pay_order_approvals WHERE order_id = ? AND revision = ? ORDER BY created_at, approver_member_id",
    )
    .bind(&row.id)
    .bind(row.revision)
    .fetch_all(pool)
    .await?;
    let min = policy.approval.min_approver_rank;
    let ids = rule.payee_ids();
    let mut out = Vec::new();
    let mut counted: HashSet<String> = HashSet::new();
    for (m, at) in rows {
        let rank_ok = match directory.rank_of(&m) {
            Some(r) => r >= min,
            None => min <= 0,
        };
        let reason = if m == row.created_by {
            Some("the creator never approves their own order".to_string())
        } else if m == row.revised_by {
            Some("the author of this revision never approves it".to_string())
        } else if member_is_payee_of(pool, directory, &m, &[], ids.as_deref()).await? {
            Some("a payee of the order".to_string())
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
        out.push(OrderApproval {
            approver_member_id: m,
            created_at: at,
            counts: reason.is_none(),
            reason,
        });
    }
    let n = counted.len() as i64;
    Ok((out, n))
}

fn required(policy: &PayoutPolicy) -> i64 {
    policy.approval.approvals_required.max(1)
}

async fn view(
    pool: &SqlitePool,
    cfg: &PaymentConfig,
    directory: &dyn Directory,
    row: OrderRow,
) -> Result<OrderView, PayoutError> {
    let eff = policy::load_effective(pool, &row.org_id, cfg).await?;
    let rule_v: Value = serde_json::from_str(&row.amount_rule).unwrap_or(Value::Null);
    let sched_v: Value = serde_json::from_str(&row.schedule).unwrap_or(Value::Null);
    let (approvals, counted) = match row.rule() {
        Ok(rule) => order_approvals(pool, directory, &eff.policy, &row, &rule).await?,
        Err(_) => (Vec::new(), 0),
    };
    let occurrences = sqlx::query_as::<_, OccurrenceRow>(
        "SELECT id, period_key, occurs_at, period_start, period_end, status, order_revision, pool_cents, paid_cents,
         leftover_cents, detail, created_at FROM pay_order_occurrences WHERE order_id = ? ORDER BY period_key DESC LIMIT 100",
    )
    .bind(&row.id)
    .fetch_all(pool)
    .await?;
    let need = required(&eff.policy);
    Ok(OrderView {
        id: row.id,
        name: row.name,
        status: row.status,
        currency: row.currency,
        rule: rule_v,
        schedule: sched_v,
        memo: row.memo,
        revision: row.revision,
        revised_by: row.revised_by,
        resume_from: row.resume_from,
        created_by: row.created_by,
        created_at: row.created_at,
        updated_by: row.updated_by,
        updated_at: row.updated_at,
        approvals,
        approvals_counted: counted,
        approvals_required: need,
        approved: counted >= need,
        occurrences,
    })
}

// ------------------------------------------------------------------ validation with the database

async fn validate_full(
    pool: &SqlitePool,
    rule: &AmountRule,
    schedule: &OrderSchedule,
) -> Result<(), PayoutError> {
    let mut errs = rule.validate();
    errs.extend(schedule.validate());
    if let Some(ids) = rule.payee_ids() {
        for id in ids {
            match payees::load(pool, &id).await? {
                None => errs.push(format!("payee {id} not found")),
                Some(p) if p.row.status == "closed" => errs.push(format!("payee {id} is closed")),
                Some(_) => {}
            }
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(PayoutError::Invalid(errs))
    }
}

async fn currency_for(
    pool: &SqlitePool,
    cfg: &PaymentConfig,
    org: &str,
    asked: Option<&str>,
) -> Result<String, PayoutError> {
    let eff = policy::load_effective(pool, org, cfg).await?;
    let c = asked
        .map(|c| c.trim().to_lowercase())
        .unwrap_or_else(|| eff.policy.currency.to_lowercase());
    if !c.eq_ignore_ascii_case(&eff.policy.currency) {
        return Err(PayoutError::Invalid(vec![format!(
            "currency must be the payout policy currency {}",
            eff.policy.currency
        )]));
    }
    Ok(c)
}

fn check_name(name: &str) -> Result<String, PayoutError> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > 200 {
        return Err(PayoutError::Invalid(vec![
            "name is required (at most 200 characters)".into(),
        ]));
    }
    Ok(n.to_string())
}

// ------------------------------------------------------------------ REST / MCP functions

/// GET /api/v1/payorders (`payorders.read`).
pub async fn list(
    pool: &SqlitePool,
    cfg: &PaymentConfig,
    directory: &dyn Directory,
    principal: &Principal,
    status: Option<&str>,
) -> Result<Vec<OrderView>, PayoutError> {
    principal.require(scopes::PAYORDERS_READ)?;
    let rows =
        match status {
            Some(s) => sqlx::query_as::<_, OrderRow>(
                "SELECT * FROM pay_orders WHERE org_id = ? AND status = ? ORDER BY created_at, id",
            )
            .bind(&principal.org_id)
            .bind(s)
            .fetch_all(pool)
            .await?,
            None => {
                sqlx::query_as::<_, OrderRow>(
                    "SELECT * FROM pay_orders WHERE org_id = ? ORDER BY created_at, id",
                )
                .bind(&principal.org_id)
                .fetch_all(pool)
                .await?
            }
        };
    let mut out = Vec::new();
    for r in rows {
        out.push(view(pool, cfg, directory, r).await?);
    }
    Ok(out)
}

/// GET /api/v1/payorders/{id} (`payorders.read`).
pub async fn get(
    pool: &SqlitePool,
    cfg: &PaymentConfig,
    directory: &dyn Directory,
    principal: &Principal,
    id: &str,
) -> Result<OrderView, PayoutError> {
    principal.require(scopes::PAYORDERS_READ)?;
    let row = load_scoped(pool, principal, id).await?;
    view(pool, cfg, directory, row).await
}

/// POST /api/v1/payorders (`payorders.manage`). The creator is the token's member and can never
/// approve the order. A payee of the order cannot create it.
pub async fn create(
    pool: &SqlitePool,
    cfg: &PaymentConfig,
    directory: &dyn Directory,
    principal: &Principal,
    input: NewOrder,
    now: DateTime<Utc>,
) -> Result<OrderView, PayoutError> {
    principal.require(scopes::PAYORDERS_MANAGE)?;
    let name = check_name(&input.name)?;
    validate_full(pool, &input.rule, &input.schedule).await?;
    refuse_payee(pool, directory, principal, &input.rule, "create").await?;
    let currency = currency_for(pool, cfg, &principal.org_id, input.currency.as_deref()).await?;
    let id = Uuid::new_v4().to_string();
    let now_s = format_db_time(now);
    let status = if input.draft { DRAFT } else { ACTIVE };
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO pay_orders (id, org_id, name, status, currency, amount_rule, schedule, memo, revision, revised_by,
         created_by, created_at, updated_by, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&principal.org_id)
    .bind(&name)
    .bind(status)
    .bind(&currency)
    .bind(serde_json::to_string(&input.rule).expect("serializable"))
    .bind(serde_json::to_string(&input.schedule).expect("serializable"))
    .bind(input.memo.as_deref().map(str::trim).filter(|m| !m.is_empty()))
    .bind(&principal.member_id)
    .bind(&principal.member_id)
    .bind(&now_s)
    .bind(&principal.member_id)
    .bind(&now_s)
    .execute(&mut *tx)
    .await?;
    event(
        &mut tx,
        &id,
        "create",
        None,
        Some(status),
        1,
        &principal.member_id,
        &now_s,
    )
    .await?;
    tx.commit().await?;
    let row = load_scoped(pool, principal, &id).await?;
    view(pool, cfg, directory, row).await
}

/// PATCH /api/v1/payorders/{id} (`payorders.manage`). A change to the rule, payees, schedule or
/// currency starts a new revision: the approvals of the old one no longer count, and the editor
/// is the author of the new one (who can never approve it).
pub async fn update(
    pool: &SqlitePool,
    cfg: &PaymentConfig,
    directory: &dyn Directory,
    principal: &Principal,
    id: &str,
    patch: OrderPatch,
    now: DateTime<Utc>,
) -> Result<OrderView, PayoutError> {
    principal.require(scopes::PAYORDERS_MANAGE)?;
    let row = load_scoped(pool, principal, id).await?;
    if row.status == ENDED || row.status == CANCELLED {
        return Err(PayoutError::InvalidState(format!(
            "pay order is '{}' and cannot be edited",
            row.status
        )));
    }
    let old_rule = row.rule()?;
    refuse_payee(pool, directory, principal, &old_rule, "edit").await?;
    let rule = patch.rule.clone().unwrap_or_else(|| old_rule.clone());
    let schedule = match &patch.schedule {
        Some(s) => s.clone(),
        None => row.sched()?,
    };
    validate_full(pool, &rule, &schedule).await?;
    refuse_payee(pool, directory, principal, &rule, "edit").await?;
    let currency = match &patch.currency {
        Some(c) => currency_for(pool, cfg, &principal.org_id, Some(c)).await?,
        None => row.currency.clone(),
    };
    let name = match &patch.name {
        Some(n) => check_name(n)?,
        None => row.name.clone(),
    };
    let memo = match &patch.memo {
        Some(m) => m
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string),
        None => row.memo.clone(),
    };
    let rule_s = serde_json::to_string(&rule).expect("serializable");
    let sched_s = serde_json::to_string(&schedule).expect("serializable");
    let material = rule_s != row.amount_rule || sched_s != row.schedule || currency != row.currency;
    let (revision, revised_by) = if material {
        (row.revision + 1, principal.member_id.clone())
    } else {
        (row.revision, row.revised_by.clone())
    };
    let now_s = format_db_time(now);
    let mut tx = pool.begin().await?;
    let res = sqlx::query(
        "UPDATE pay_orders SET name = ?, memo = ?, currency = ?, amount_rule = ?, schedule = ?, revision = ?, revised_by = ?,
         updated_by = ?, updated_at = ? WHERE id = ? AND revision = ? AND status NOT IN ('ended', 'cancelled')",
    )
    .bind(&name)
    .bind(&memo)
    .bind(&currency)
    .bind(&rule_s)
    .bind(&sched_s)
    .bind(revision)
    .bind(&revised_by)
    .bind(&principal.member_id)
    .bind(&now_s)
    .bind(id)
    .bind(row.revision)
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() == 0 {
        return Err(PayoutError::InvalidState(
            "pay order changed concurrently; read it again".into(),
        ));
    }
    event(
        &mut tx,
        id,
        "update",
        Some(&row.status),
        Some(&row.status),
        revision,
        &principal.member_id,
        &now_s,
    )
    .await?;
    tx.commit().await?;
    let row = load_scoped(pool, principal, id).await?;
    view(pool, cfg, directory, row).await
}

#[derive(Debug, Clone, Serialize)]
pub struct ApproveOrderResult {
    /// recorded | already_approved
    pub result: &'static str,
    pub approver_member_id: String,
    pub revision: i64,
    pub order: OrderView,
}

/// POST /api/v1/payorders/{id}/approve (`payorders.approve`). Records the token member's approval
/// of the current revision. Refused for the creator, the author of the revision and any payee of
/// the order. An approval below min_approver_rank is recorded but does not count.
pub async fn approve(
    pool: &SqlitePool,
    cfg: &PaymentConfig,
    directory: &dyn Directory,
    principal: &Principal,
    id: &str,
    now: DateTime<Utc>,
) -> Result<ApproveOrderResult, PayoutError> {
    principal.require(scopes::PAYORDERS_APPROVE)?;
    let row = load_scoped(pool, principal, id).await?;
    if row.status == ENDED || row.status == CANCELLED {
        return Err(PayoutError::InvalidState(format!(
            "pay order is '{}' and cannot be approved",
            row.status
        )));
    }
    if principal.member_id == row.created_by {
        return Err(PayoutError::Forbidden(
            "the creator of a pay order can never approve it".into(),
        ));
    }
    if principal.member_id == row.revised_by {
        return Err(PayoutError::Forbidden(
            "the author of a pay order change can never approve it".into(),
        ));
    }
    refuse_payee(pool, directory, principal, &row.rule()?, "approve").await?;
    let mut tx = pool.begin().await?;
    let res = sqlx::query(
        "INSERT OR IGNORE INTO pay_order_approvals (order_id, revision, approver_member_id, org_id, token_id, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(row.revision)
    .bind(&principal.member_id)
    .bind(&principal.org_id)
    .bind(&principal.token_id)
    .bind(format_db_time(now))
    .execute(&mut *tx)
    .await?;
    let recorded = res.rows_affected() > 0;
    if recorded {
        event(
            &mut tx,
            id,
            "approve",
            Some(&row.status),
            Some(&row.status),
            row.revision,
            &principal.member_id,
            &format_db_time(now),
        )
        .await?;
    }
    tx.commit().await?;
    let revision = row.revision;
    let order = view(pool, cfg, directory, row).await?;
    Ok(ApproveOrderResult {
        result: if recorded {
            "recorded"
        } else {
            "already_approved"
        },
        approver_member_id: principal.member_id.clone(),
        revision,
        order,
    })
}

/// pause | resume | cancel (`payorders.manage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    Pause,
    Resume,
    Cancel,
}

/// POST /api/v1/payorders/{id}/pause|resume|cancel (`payorders.manage`). Pausing skips the
/// occurrences that fall while paused (they are never paid later). Resume starts a draft or
/// continues a paused order. Cancel is final. A payee of the order can do none of these.
pub async fn transition(
    pool: &SqlitePool,
    cfg: &PaymentConfig,
    directory: &dyn Directory,
    principal: &Principal,
    id: &str,
    t: Transition,
    now: DateTime<Utc>,
) -> Result<OrderView, PayoutError> {
    principal.require(scopes::PAYORDERS_MANAGE)?;
    let row = load_scoped(pool, principal, id).await?;
    let verb = match t {
        Transition::Pause => "pause",
        Transition::Resume => "resume",
        Transition::Cancel => "cancel",
    };
    refuse_payee(pool, directory, principal, &row.rule()?, verb).await?;
    let (from, to): (&[&str], &str) = match t {
        Transition::Pause => (&[ACTIVE], PAUSED),
        Transition::Resume => (&[PAUSED, DRAFT], ACTIVE),
        Transition::Cancel => (&[DRAFT, ACTIVE, PAUSED], CANCELLED),
    };
    if !from.contains(&row.status.as_str()) {
        return Err(PayoutError::InvalidState(format!(
            "pay order is '{}' and cannot {verb}",
            row.status
        )));
    }
    let now_s = format_db_time(now);
    // Resuming a paused order skips what fell while paused: nothing dated before now is paid.
    let resume_from = if t == Transition::Resume && row.status == PAUSED {
        Some(now_s.clone())
    } else {
        row.resume_from.clone()
    };
    let mut tx = pool.begin().await?;
    let res = sqlx::query("UPDATE pay_orders SET status = ?, resume_from = ?, updated_by = ?, updated_at = ? WHERE id = ? AND status = ?")
        .bind(to)
        .bind(&resume_from)
        .bind(&principal.member_id)
        .bind(&now_s)
        .bind(id)
        .bind(&row.status)
        .execute(&mut *tx)
        .await?;
    if res.rows_affected() == 0 {
        return Err(PayoutError::InvalidState(
            "pay order changed concurrently; read it again".into(),
        ));
    }
    event(
        &mut tx,
        id,
        verb,
        Some(&row.status),
        Some(to),
        row.revision,
        &principal.member_id,
        &now_s,
    )
    .await?;
    tx.commit().await?;
    let row = load_scoped(pool, principal, id).await?;
    view(pool, cfg, directory, row).await
}

// ------------------------------------------------------------------ computing an occurrence

/// Key -> payee id for every payee key.
async fn key_map(pool: &SqlitePool) -> Result<HashMap<String, String>, sqlx::Error> {
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT payee_key, payee_id FROM payee_keys")
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().collect())
}

fn norm_id(col: &str) -> String {
    format!(
        "(CASE WHEN typeof({c}) = 'blob' THEN lower(substr(hex({c}),1,8)||'-'||substr(hex({c}),9,4)||'-'||substr(hex({c}),13,4)||'-'||substr(hex({c}),17,4)||'-'||substr(hex({c}),21)) ELSE lower({c}) END)",
        c = col
    )
}

fn default_host(provider: &str) -> &'static str {
    if provider == "gitlab" {
        "gitlab.com"
    } else {
        "github.com"
    }
}

/// XP earned in [start, end) from `merit_activities` (by created_at), with the rule's
/// repository, role and payee filters applied. One entry per person.
pub async fn xp_in_period(
    pool: &SqlitePool,
    directory: &dyn Directory,
    rule: &PoolByXpRule,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<XpInput>, sqlx::Error> {
    let sql = format!(
        "SELECT c.github_username AS login, c.current_role AS role, a.provider AS provider, a.host AS host, a.repo AS repo,
                SUM(a.merit_change) AS pts
         FROM merit_activities a JOIN contributors c ON {cid} = {aid}
         WHERE datetime(a.created_at) >= datetime(?) AND datetime(a.created_at) < datetime(?)
         GROUP BY c.github_username, c.current_role, a.provider, a.host, a.repo",
        cid = norm_id("c.id"),
        aid = norm_id("a.contributor_id")
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(format_db_time(start))
        .bind(format_db_time(end))
        .fetch_all(pool)
        .await?;
    let keys = key_map(pool).await?;
    let mut per: BTreeMap<String, (String, i64)> = BTreeMap::new();
    for r in rows {
        let login: String = r.try_get("login")?;
        let role: String = r.try_get("role")?;
        let provider: Option<String> = r.try_get("provider")?;
        let host: Option<String> = r.try_get("host")?;
        let repo: Option<String> = r.try_get("repo")?;
        let pts: i64 = r.try_get::<Option<i64>, _>("pts")?.unwrap_or(0);
        if !rule.repos.is_empty() {
            let hit = match (&provider, &repo) {
                (Some(p), Some(rp)) => rule.repos.iter().any(|f| {
                    f.provider == *p
                        && f.repo.trim().eq_ignore_ascii_case(rp)
                        && f.host
                            .as_deref()
                            .unwrap_or(default_host(&f.provider))
                            .eq_ignore_ascii_case(host.as_deref().unwrap_or(default_host(p)))
                }),
                _ => false,
            };
            if !hit {
                continue;
            }
        }
        let e = per.entry(login.to_lowercase()).or_insert((role, 0));
        e.1 = e.1.saturating_add(pts);
    }
    let mut out = Vec::new();
    for (key, (role, xp)) in per {
        let payee_id = keys.get(&key).cloned();
        if !rule.payee_ids.is_empty()
            && !payee_id
                .as_ref()
                .is_some_and(|p| rule.payee_ids.contains(p))
        {
            continue;
        }
        if !rule.roles.is_empty() {
            let mut member = directory.member_for_payee(&key);
            if member.is_none() {
                if let Some(pid) = &payee_id {
                    member = payees::load(pool, pid).await?.and_then(|p| p.row.member_id);
                }
            }
            let cra_roles = member
                .as_deref()
                .map(|m| directory.roles_of(m))
                .unwrap_or_default();
            let ok = rule.roles.iter().any(|want| {
                want.eq_ignore_ascii_case(&role)
                    || cra_roles.iter().any(|r| r.eq_ignore_ascii_case(want))
            });
            if !ok {
                continue;
            }
        }
        out.push(XpInput { key, payee_id, xp });
    }
    Ok(out)
}

/// The split for one occurrence, or Err(reason) when it cannot be computed yet (a
/// `percent_of_amount` occurrence with no base).
pub async fn compute_split(
    pool: &SqlitePool,
    directory: &dyn Directory,
    rule: &AmountRule,
    occ: &Occurrence,
) -> Result<Result<Split, String>, sqlx::Error> {
    Ok(match rule {
        AmountRule::Fixed(r) => split_fixed(&r.payee_id, r.amount_cents),
        AmountRule::PoolPercent(r) => {
            let shares: Vec<(String, i64)> = r
                .shares
                .iter()
                .map(|s| (s.payee_id.clone(), s.bps))
                .collect();
            split_pool_percent(r.pool_cents, &shares)
        }
        AmountRule::PoolByXp(r) => {
            let xp = xp_in_period(pool, directory, r, occ.period_start, occ.period_end).await?;
            split_pool_by_xp(r.pool_cents, &xp, r.min_xp, r.max_share_bps)
        }
        AmountRule::PercentOfAmount(r) => {
            match r.bases.get(&occ.period_key).copied().or(r.base_cents) {
                Some(base) => split_percent_of_amount(&r.payee_id, base, r.bps),
                None => Err(format!(
                    "no base amount for {}; add it to the order (bases or base_cents)",
                    occ.period_key
                )),
            }
        }
    })
}

/// Withhold the shares of payees that are paused or closed (never queued for later).
async fn withhold_inactive(
    pool: &SqlitePool,
    split: &mut Split,
) -> Result<HashMap<String, Payee>, sqlx::Error> {
    let mut found = HashMap::new();
    let ids: Vec<String> = split
        .allocations
        .iter()
        .filter_map(|a| a.payee_id.clone())
        .collect();
    for id in ids {
        match payees::load(pool, &id).await? {
            Some(p) => {
                if !p.is_active() {
                    split.withhold(&id, &format!("payee is {}; share not paid", p.row.status));
                }
                found.insert(id, p);
            }
            None => split.withhold(&id, "payee not found; share not paid"),
        }
    }
    Ok(found)
}

// ------------------------------------------------------------------ preview

#[derive(Debug, Clone, Serialize)]
pub struct PreviewItem {
    pub period_key: String,
    pub occurs_at: String,
    pub period_start: String,
    pub period_end: String,
    /// due (date passed, not yet materialized) | upcoming
    pub timing: &'static str,
    pub split: Option<Split>,
    /// Why this occurrence would not materialize now (approvals, status, missing base).
    pub waiting_for: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Preview {
    pub order_id: String,
    pub status: String,
    pub approved: bool,
    pub items: Vec<PreviewItem>,
    /// pool_by_xp shares of future periods count only the XP earned so far.
    pub note: Option<String>,
}

/// GET /api/v1/payorders/{id}/preview?periods=N (`payorders.read`). What the next N occurrences
/// would pay. Persists nothing.
pub async fn preview(
    pool: &SqlitePool,
    cfg: &PaymentConfig,
    directory: &dyn Directory,
    principal: &Principal,
    id: &str,
    periods: u32,
    now: DateTime<Utc>,
) -> Result<Preview, PayoutError> {
    principal.require(scopes::PAYORDERS_READ)?;
    let periods = periods.clamp(1, MAX_PREVIEW_PERIODS);
    let row = load_scoped(pool, principal, id).await?;
    let rule = row.rule()?;
    let schedule = row.sched()?;
    let eff = policy::load_effective(pool, &row.org_id, cfg).await?;
    let (_, counted) = order_approvals(pool, directory, &eff.policy, &row, &rule).await?;
    let approved = counted >= required(&eff.policy);
    let recorded = recorded_keys(pool, &row.id).await?;
    let resume = row.resume_from.as_deref().and_then(parse_db_time);
    let mut items = Vec::new();
    for occ in upcoming(&schedule, resume, &recorded, periods) {
        let mut waiting = Vec::new();
        if row.status != ACTIVE {
            waiting.push(format!("order is {}", row.status));
        }
        if !approved {
            waiting.push(format!(
                "{counted} of {} required approvals",
                required(&eff.policy)
            ));
        }
        let split = match compute_split(pool, directory, &rule, &occ).await? {
            Ok(mut s) => {
                withhold_inactive(pool, &mut s).await?;
                Some(s)
            }
            Err(reason) => {
                waiting.push(reason);
                None
            }
        };
        items.push(PreviewItem {
            period_key: occ.period_key.clone(),
            occurs_at: format_db_time(occ.occurs_at),
            period_start: format_db_time(occ.period_start),
            period_end: format_db_time(occ.period_end),
            timing: if occ.occurs_at <= now {
                "due"
            } else {
                "upcoming"
            },
            split,
            waiting_for: waiting,
        });
    }
    Ok(Preview {
        order_id: row.id,
        status: row.status,
        approved,
        items,
        note: matches!(rule, AmountRule::PoolByXp(_))
            .then(|| "pool_by_xp shares count the XP earned so far in each period".into()),
    })
}

async fn recorded_keys(pool: &SqlitePool, order_id: &str) -> Result<HashSet<String>, sqlx::Error> {
    let keys: Vec<String> =
        sqlx::query_scalar("SELECT period_key FROM pay_order_occurrences WHERE order_id = ?")
            .bind(order_id)
            .fetch_all(pool)
            .await?;
    Ok(keys.into_iter().collect())
}

// ------------------------------------------------------------------ materialize

/// What one materialization pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MaterializeReport {
    /// (order id, period key, ledger row ids written)
    pub materialized: Vec<(String, String, Vec<String>)>,
    /// (order id, period key): fell while the order was paused.
    pub skipped: Vec<(String, String)>,
    /// (order id, reason): due occurrences waiting (approvals, missing base).
    pub waiting: Vec<(String, String)>,
    pub ended: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
async fn record_occurrence(
    pool: &SqlitePool,
    row: &OrderRow,
    occ: &Occurrence,
    status: &str,
    split: Option<&Split>,
    reason: Option<&str>,
    payees_found: &HashMap<String, Payee>,
    approvers: &[String],
    now: DateTime<Utc>,
) -> Result<Option<Vec<String>>, sqlx::Error> {
    let occ_id = Uuid::new_v4().to_string();
    let now_s = format_db_time(now);
    let detail = json!({ "split": split, "reason": reason });
    let mut tx = pool.begin().await?;
    let res = sqlx::query(
        "INSERT OR IGNORE INTO pay_order_occurrences (id, order_id, org_id, period_key, occurs_at, period_start, period_end, status,
         order_revision, pool_cents, paid_cents, leftover_cents, detail, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&occ_id)
    .bind(&row.id)
    .bind(&row.org_id)
    .bind(&occ.period_key)
    .bind(format_db_time(occ.occurs_at))
    .bind(format_db_time(occ.period_start))
    .bind(format_db_time(occ.period_end))
    .bind(status)
    .bind(row.revision)
    .bind(split.map_or(0, |s| s.pool_cents))
    .bind(split.map_or(0, |s| s.paid_cents))
    .bind(split.map_or(0, |s| s.leftover_cents))
    .bind(detail.to_string())
    .bind(&now_s)
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() == 0 {
        // Already recorded by an earlier tick (or a concurrent one): never twice.
        tx.rollback().await?;
        return Ok(None);
    }
    let mut ids = Vec::new();
    if let Some(split) = split {
        for a in &split.allocations {
            let Some(pid) = &a.payee_id else { continue };
            if a.cents <= 0 {
                continue;
            }
            let Some(p) = payees_found.get(pid) else {
                continue;
            };
            let contributor_id: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT {} FROM contributors c JOIN payee_keys k ON k.payee_key = lower(c.github_username) WHERE k.payee_id = ? ORDER BY k.payee_key LIMIT 1",
                norm_id("c.id")
            )))
            .bind(pid)
            .fetch_optional(&mut *tx)
            .await?;
            let src = SourceRow {
                source_kind: SOURCE_PAY_ORDER,
                source_ref: occ_id.clone(),
                payee_id: pid.clone(),
                payee_key: p.ledger_key(),
                contributor_id,
                title: format!("{} ({})", row.name, occ.period_key),
                amount_cents: a.cents,
                currency: row.currency.clone(),
                pay_after: Some(format_db_time(occ.occurs_at)),
                created_at: now_s.clone(),
            };
            let Some(lid) = ledger::insert_source_row(&mut tx, &src).await? else {
                continue;
            };
            // The order's counted approvals carry over to its rows; evaluate re-checks each
            // approver (rank, payee) at pay time, exactly as for merit rows.
            for m in approvers {
                sqlx::query("INSERT OR IGNORE INTO payout_approvals (ledger_id, approver_member_id, org_id, token_id, created_at) VALUES (?, ?, ?, ?, ?)")
                    .bind(&lid)
                    .bind(m)
                    .bind(&row.org_id)
                    .bind(format!("pay_order:{}:r{}", row.id, row.revision))
                    .bind(&now_s)
                    .execute(&mut *tx)
                    .await?;
            }
            ids.push(lid);
        }
    }
    crate::audit::append_in(
        &mut tx,
        &crate::audit::AuditEntry::new(
            crate::audit::ActorKind::System,
            "system:scheduler",
            "payorder.occurrence",
            format!("payorder:{}", row.id),
            serde_json::json!({
                "order_id": row.id,
                "occurrence_id": occ_id,
                "period_key": occ.period_key,
                "status": status,
                "revision": row.revision,
                "ledger_ids": ids,
                "approvers": approvers,
                "paid_cents": split.map_or(0, |s| s.paid_cents),
                "leftover_cents": split.map_or(0, |s| s.leftover_cents),
                "reason": reason,
            }),
        ),
    )
    .await?;
    tx.commit().await?;
    Ok(Some(ids))
}

/// Materialize every due occurrence of every active, approved order at `now`. Creates ledger
/// rows only; never transfers. Safe to call any number of times: `UNIQUE (order_id, period_key)`.
pub async fn materialize_due(
    ctx: &ApiContext,
    now: DateTime<Utc>,
) -> Result<MaterializeReport, PayoutError> {
    let pool = &ctx.pool;
    let directory = ctx.directory.as_ref();
    let mut report = MaterializeReport::default();
    let orders = sqlx::query_as::<_, OrderRow>(
        "SELECT * FROM pay_orders WHERE status = ? ORDER BY created_at, id",
    )
    .bind(ACTIVE)
    .fetch_all(pool)
    .await?;
    for row in orders {
        let (rule, schedule) = match (row.rule(), row.sched()) {
            (Ok(r), Ok(s)) => (r, s),
            _ => {
                error!(
                    "pay order {} has an unreadable rule or schedule; skipped",
                    row.id
                );
                continue;
            }
        };
        let eff = policy::load_effective(pool, &row.org_id, &ctx.payments).await?;
        let recorded = recorded_keys(pool, &row.id).await?;
        let resume = row.resume_from.as_deref().and_then(parse_db_time);
        let plan = due_occurrences(&schedule, resume, now, &recorded);

        for occ in &plan.skipped {
            let done = record_occurrence(
                pool,
                &row,
                occ,
                "skipped",
                None,
                Some("the order was paused on this date"),
                &HashMap::new(),
                &[],
                now,
            )
            .await?;
            if done.is_some() {
                report
                    .skipped
                    .push((row.id.clone(), occ.period_key.clone()));
            }
        }

        let mut pending = false;
        if !plan.due.is_empty() {
            let (approvals, counted) =
                order_approvals(pool, directory, &eff.policy, &row, &rule).await?;
            if counted < required(&eff.policy) {
                report.waiting.push((
                    row.id.clone(),
                    format!("{counted} of {} required approvals", required(&eff.policy)),
                ));
                continue;
            }
            let approvers: Vec<String> = approvals
                .into_iter()
                .filter(|a| a.counts)
                .map(|a| a.approver_member_id)
                .collect();
            for occ in &plan.due {
                let mut split = match compute_split(pool, directory, &rule, occ).await? {
                    Ok(s) => s,
                    Err(reason) => {
                        pending = true;
                        report.waiting.push((row.id.clone(), reason));
                        continue;
                    }
                };
                let found = withhold_inactive(pool, &mut split).await?;
                if let Some(ids) = record_occurrence(
                    pool,
                    &row,
                    occ,
                    "materialized",
                    Some(&split),
                    split.note.as_deref(),
                    &found,
                    &approvers,
                    now,
                )
                .await?
                {
                    info!(
                        "pay order {} occurrence {} materialized into {} ledger rows",
                        row.id,
                        occ.period_key,
                        ids.len()
                    );
                    report
                        .materialized
                        .push((row.id.clone(), occ.period_key.clone(), ids));
                }
            }
        }

        if plan.schedule_exhausted && !pending {
            let mut tx = pool.begin().await?;
            let res = sqlx::query("UPDATE pay_orders SET status = ?, updated_by = ?, updated_at = ? WHERE id = ? AND status = ?")
                .bind(ENDED)
                .bind("system:scheduler")
                .bind(format_db_time(now))
                .bind(&row.id)
                .bind(ACTIVE)
                .execute(&mut *tx)
                .await?;
            if res.rows_affected() > 0 {
                event(
                    &mut tx,
                    &row.id,
                    "end",
                    Some(ACTIVE),
                    Some(ENDED),
                    row.revision,
                    "system:scheduler",
                    &format_db_time(now),
                )
                .await?;
                report.ended.push(row.id.clone());
            }
            tx.commit().await?;
        }
    }
    if !report.waiting.is_empty() {
        warn!("pay orders waiting: {:?}", report.waiting);
    }
    Ok(report)
}
