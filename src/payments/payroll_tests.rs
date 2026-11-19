//! Payroll control tests (contract 11): money arithmetic, schedules, authority, invoices, ledger
//! sources and the REST surface. Stripe is the mock from `tests.rs`; nothing here calls live
//! Stripe.

use super::api::{self, ApiContext};
use super::approvals::{self, Blocker};
use super::error::PayoutError;
use super::invoices::{self, check_total, InvoiceLine};
use super::ledger::{self, LedgerError, SourceRow, PAID, PENDING, REJECTED, SOURCE_INVOICE, SOURCE_MERIT, SOURCE_PAY_ORDER};
use super::payees;
use super::payorders::{
    self, due_occurrences, occurrence, percent_of, split_fixed, split_percent_of_amount, split_pool_by_xp, split_pool_percent,
    AmountRule, OrderSchedule, Transition, XpInput, BPS_FULL,
};
use super::policy::{self, PayoutPolicy};
use super::runs::{self, CreateRunResult};
use super::schedule;
use super::statement;
use super::tests::{add_contributor, cfg, pool, MockStripe};
use crate::auth::{create_token, scopes, EnvDirectory, NewToken, Principal, TokenCreator};
use actix_web::{test as atest, web, App};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::collections::HashSet;
use std::sync::Arc;

// ================================================================== helpers

fn dir() -> EnvDirectory {
    EnvDirectory::new(vec!["owner1".to_string(), "owner2".to_string(), "owner3".to_string()])
}

const PAYROLL: &[&str] = &[
    scopes::PAYEES_READ,
    scopes::PAYEES_MANAGE,
    scopes::PAYORDERS_READ,
    scopes::PAYORDERS_MANAGE,
    scopes::PAYORDERS_APPROVE,
    scopes::INVOICES_SUBMIT,
    scopes::INVOICES_READ,
    scopes::INVOICES_APPROVE,
    scopes::PAYOUTS_READ,
];

fn who(member: &str, keys: &[&str]) -> Principal {
    Principal {
        token_id: format!("tok-{member}"),
        org_id: "default".into(),
        member_id: member.into(),
        scopes: PAYROLL.iter().map(|s| s.to_string()).collect(),
        payee_keys: keys.iter().map(|s| s.to_string()).collect(),
    }
}

fn at(y: i32, m: u32, d: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, 12, 0, 0).unwrap()
}

fn date(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
}

fn base_policy() -> PayoutPolicy {
    serde_json::from_value(json!({
        "schema": 1, "enabled": true, "currency": "usd",
        "schedule": {"cadence": "manual"},
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
async fn payee(pool: &SqlitePool, name: &str, keys: &[&str], member: Option<&str>) -> String {
    let p = payees::create(
        pool,
        &dir(),
        &who("staff", &[]),
        serde_json::from_value(json!({
            "display_name": name, "kind": "individual", "payee_keys": keys, "member_id": member,
            "stripe_account_id": format!("acct_{}", name.to_lowercase()),
        }))
        .unwrap(),
        Utc::now(),
    )
    .await
    .unwrap();
    ledger::update_account_status(pool, &format!("acct_{}", name.to_lowercase()), true).await.unwrap();
    p.row.id
}

async fn order(pool: &SqlitePool, by: &Principal, rule: Value, schedule: Value) -> Result<payorders::OrderView, PayoutError> {
    let input: payorders::NewOrder = serde_json::from_value(json!({"name": "Order", "rule": rule, "schedule": schedule})).unwrap();
    payorders::create(pool, &cfg(), &dir(), by, input, Utc::now()).await
}

async fn approve(pool: &SqlitePool, id: &str, by: &str) -> Result<payorders::ApproveOrderResult, PayoutError> {
    payorders::approve(pool, &cfg(), &dir(), &who(by, &[]), id, Utc::now()).await
}

fn ctx(pool: &SqlitePool, stripe: Arc<MockStripe>) -> ApiContext {
    ApiContext { pool: pool.clone(), payments: Arc::new(cfg()), stripe, directory: Arc::new(dir()) }
}

async fn materialize(pool: &SqlitePool, now: DateTime<Utc>) -> payorders::MaterializeReport {
    payorders::materialize_due(&ctx(pool, Arc::new(MockStripe::default())), now).await.unwrap()
}

async fn source_rows(pool: &SqlitePool) -> Vec<ledger::LedgerEntry> {
    sqlx::query_as::<_, ledger::LedgerEntry>("SELECT * FROM payout_ledger WHERE source_kind <> 'merit' ORDER BY pay_after, id")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn count(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

/// Prepare and execute a run as the legacy admin at `now`. Returns the execute report.
async fn pay_run(pool: &SqlitePool, stripe: &MockStripe, now: DateTime<Utc>) -> runs::ExecuteReport {
    let id = match runs::create_run(pool, &cfg(), &dir(), &Principal::legacy_admin(), false, now).await.unwrap() {
        CreateRunResult::Created { run, .. } => run.run.id,
        CreateRunResult::DryRun(_) => panic!("expected a run"),
    };
    runs::execute_run(pool, stripe, &cfg(), &dir(), &Principal::legacy_admin(), &id, now).await.unwrap()
}

fn transfers(s: &MockStripe) -> usize {
    s.transfers.lock().unwrap().len()
}

fn fixed(payee: &str, cents: i64) -> Value {
    json!({"kind": "fixed", "payee_id": payee, "amount_cents": cents})
}

fn once(d: &str) -> Value {
    json!({"kind": "once", "date": d})
}

fn monthly(start: &str) -> Value {
    json!({"kind": "repeat", "cadence": "monthly", "start": start})
}

// ================================================================== arithmetic (pure)

#[test]
fn fixed_pays_exactly_the_amount() {
    let s = split_fixed("p1", 250_000).unwrap();
    assert_eq!(s.allocations.len(), 1);
    assert_eq!(s.allocations[0].cents, 250_000);
    assert_eq!((s.paid_cents, s.leftover_cents), (250_000, 0));
    assert!(split_fixed("p1", 0).is_err());
    assert!(split_fixed("p1", -5).is_err());
}

#[test]
fn pool_percent_rounds_down_per_payee_and_reports_the_leftover() {
    let thirds = vec![("a".to_string(), 3333), ("b".to_string(), 3333), ("c".to_string(), 3334)];
    let s = split_pool_percent(1000, &thirds).unwrap();
    let cents: Vec<i64> = s.allocations.iter().map(|a| a.cents).collect();
    assert_eq!(cents, vec![333, 333, 333], "floor(1000 * 3334 / 10000) = 333");
    assert_eq!((s.paid_cents, s.leftover_cents), (999, 1));

    let s = split_pool_percent(10_001, &[("a".to_string(), 5000), ("b".to_string(), 2500)]).unwrap();
    assert_eq!(s.allocations.iter().map(|a| a.cents).collect::<Vec<_>>(), vec![5000, 2500]);
    assert_eq!(s.leftover_cents, 2501, "the unassigned 25% and the rounding stay unpaid");
}

#[test]
fn pool_percent_over_one_hundred_percent_is_refused() {
    assert!(split_pool_percent(1000, &[("a".to_string(), 6000), ("b".to_string(), 4001)]).is_err());
    assert!(split_pool_percent(1000, &[("a".to_string(), 10_001)]).is_err());
    assert!(split_pool_percent(1000, &[("a".to_string(), 6000), ("b".to_string(), 4000)]).is_ok(), "exactly 100% is allowed");
    let rule: AmountRule = serde_json::from_value(json!({"kind": "pool_percent", "pool_cents": 100,
        "shares": [{"payee_id": "a", "bps": 6000}, {"payee_id": "b", "bps": 4001}]})).unwrap();
    assert!(rule.validate().iter().any(|e| e.contains("10000")), "{:?}", rule.validate());
}

fn xp(key: &str, payee: Option<&str>, xp: i64) -> XpInput {
    XpInput { key: key.into(), payee_id: payee.map(str::to_string), xp }
}

#[test]
fn pool_by_xp_splits_in_proportion() {
    let s = split_pool_by_xp(10_000, &[xp("a", Some("pa"), 30), xp("b", Some("pb"), 10)], 0, None).unwrap();
    let by: Vec<(Option<String>, i64)> = s.allocations.iter().map(|a| (a.payee_id.clone(), a.cents)).collect();
    assert_eq!(by, vec![(Some("pa".into()), 7500), (Some("pb".into()), 2500)]);
    assert_eq!(s.leftover_cents, 0);
    // rounding: 100 split 1:1:1 is 33 each, 1 leftover
    let s = split_pool_by_xp(100, &[xp("a", Some("pa"), 1), xp("b", Some("pb"), 1), xp("c", Some("pc"), 1)], 0, None).unwrap();
    assert_eq!(s.paid_cents, 99);
    assert_eq!(s.leftover_cents, 1);
    // two keys of one payee add up
    let s = split_pool_by_xp(900, &[xp("a", Some("pa"), 10), xp("gitlab:a", Some("pa"), 20), xp("b", Some("pb"), 30)], 0, None).unwrap();
    assert_eq!(s.allocations.iter().map(|a| a.cents).collect::<Vec<_>>(), vec![450, 450]);
    // a person with no payee record: share computed, not paid
    let s = split_pool_by_xp(1000, &[xp("a", Some("pa"), 50), xp("z", None, 50)], 0, None).unwrap();
    assert_eq!(s.paid_cents, 500);
    assert_eq!(s.leftover_cents, 500);
}

#[test]
fn pool_by_xp_minimum_xp_excludes_small_contributors() {
    let s = split_pool_by_xp(10_000, &[xp("a", Some("pa"), 30), xp("b", Some("pb"), 10)], 15, None).unwrap();
    assert_eq!(s.allocations.len(), 1);
    assert_eq!(s.allocations[0].payee_id.as_deref(), Some("pa"));
    assert_eq!(s.allocations[0].cents, 10_000);
}

#[test]
fn pool_by_xp_maximum_share_caps_and_does_not_redistribute() {
    let s = split_pool_by_xp(10_000, &[xp("a", Some("pa"), 30), xp("b", Some("pb"), 10)], 0, Some(5000)).unwrap();
    assert_eq!(s.allocations[0].cents, 5000);
    assert_eq!(s.allocations[0].unpaid_cents, 2500);
    assert_eq!(s.allocations[1].cents, 2500);
    assert_eq!((s.paid_cents, s.leftover_cents), (7500, 2500));
}

#[test]
fn pool_by_xp_with_zero_qualifying_xp_pays_nothing_and_says_why() {
    for entries in [vec![], vec![xp("a", Some("pa"), 0), xp("b", Some("pb"), -4)]] {
        let s = split_pool_by_xp(10_000, &entries, 0, None).unwrap();
        assert_eq!(s.paid_cents, 0);
        assert!(s.allocations.iter().all(|a| a.cents == 0));
        assert_eq!(s.leftover_cents, 10_000);
        assert!(s.note.as_deref().unwrap_or_default().contains("nothing is paid"), "{s:?}");
    }
}

#[test]
fn percent_of_amount_rounds_down_and_refuses_over_one_hundred_percent() {
    assert_eq!(percent_of(123_456, 1000).unwrap(), 12_345);
    assert_eq!(percent_of(99, 5000).unwrap(), 49);
    assert_eq!(split_percent_of_amount("p", 1_000_000, 250).unwrap().allocations[0].cents, 25_000);
    assert!(percent_of(1000, BPS_FULL + 1).is_err());
    assert!(percent_of(-1, 100).is_err());
}

#[test]
fn no_split_ever_pays_more_than_its_pool() {
    // Deterministic pseudo-random inputs (LCG), many shapes, including extremes.
    let mut seed: u64 = 0x5eed_1234;
    let mut next = |m: u64| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) % m
    };
    for i in 0..4000 {
        let pool = match i % 4 {
            0 => next(10) as i64,
            1 => next(1_000_000) as i64,
            2 => payorders::MAX_CENTS - next(1000) as i64,
            _ => next(100_000_000_000) as i64,
        };
        // pool_percent with shares summing to at most 100%
        let n = 1 + next(12) as usize;
        let mut left = BPS_FULL;
        let mut shares = Vec::new();
        for k in 0..n {
            let b = if left == 0 { 0 } else { next(left as u64 + 1) as i64 };
            left -= b;
            shares.push((format!("p{k}"), b));
        }
        let s = split_pool_percent(pool, &shares).unwrap();
        assert!(s.paid_cents <= pool && s.leftover_cents >= 0, "pool_percent {pool} {shares:?} -> {s:?}");
        assert_eq!(s.paid_cents + s.leftover_cents, pool);
        assert!(s.allocations.iter().all(|a| a.cents >= 0));

        // pool_by_xp with random XP, minimum and cap
        let mut entries: Vec<XpInput> = Vec::new();
        for k in 0..1 + next(15) {
            let payee_id = if next(5) == 0 { None } else { Some(format!("p{}", next(8))) };
            entries.push(XpInput { key: format!("k{k}"), payee_id, xp: next(10_000) as i64 - 50 });
        }
        let cap = if next(2) == 0 { None } else { Some(next(BPS_FULL as u64 + 1) as i64) };
        let s = split_pool_by_xp(pool, &entries, next(100) as i64, cap).unwrap();
        let paid: i64 = s.allocations.iter().map(|a| a.cents).sum();
        let owed: i64 = s.allocations.iter().map(|a| a.cents + a.unpaid_cents).sum();
        assert!(paid <= pool && owed <= pool && s.leftover_cents >= 0, "pool_by_xp {pool} -> {s:?}");
        assert_eq!(s.paid_cents, paid);
        if let Some(c) = cap {
            let max = percent_of(pool, c).unwrap();
            assert!(s.allocations.iter().all(|a| a.cents <= max), "cap {c}");
        }
    }
}

// ================================================================== schedules (pure)

fn sched(v: Value) -> OrderSchedule {
    serde_json::from_value(v).unwrap()
}

#[test]
fn once_fires_exactly_once() {
    let s = sched(once("2026-12-01"));
    let none = HashSet::new();
    assert!(due_occurrences(&s, None, at(2026, 11, 30), &none).due.is_empty(), "not before its date");
    let plan = due_occurrences(&s, None, at(2026, 12, 1), &none);
    assert_eq!(plan.due.iter().map(|o| o.period_key.as_str()).collect::<Vec<_>>(), vec!["2026-12-01"]);
    assert!(plan.schedule_exhausted);
    let done: HashSet<String> = ["2026-12-01".to_string()].into();
    for later in [at(2026, 12, 1), at(2027, 6, 1), at(2030, 1, 1)] {
        assert!(due_occurrences(&s, None, later, &done).due.is_empty(), "never again");
    }
    assert!(occurrence(&s, 1).is_none());
}

#[test]
fn every_repeat_cadence_steps_correctly() {
    let keys = |v: Value, n: u32| -> Vec<String> { (0..n).filter_map(|i| occurrence(&sched(v.clone()), i)).map(|o| o.period_key).collect() };
    assert_eq!(keys(json!({"kind": "repeat", "cadence": "weekly", "start": "2026-01-05"}), 3), vec!["2026-01-05", "2026-01-12", "2026-01-19"]);
    assert_eq!(keys(json!({"kind": "repeat", "cadence": "biweekly", "start": "2026-01-05"}), 3), vec!["2026-01-05", "2026-01-19", "2026-02-02"]);
    // monthly keeps the start day, clamped to short months, and never drifts
    assert_eq!(
        keys(json!({"kind": "repeat", "cadence": "monthly", "start": "2026-01-31"}), 4),
        vec!["2026-01-31", "2026-02-28", "2026-03-31", "2026-04-30"]
    );
    assert_eq!(
        keys(json!({"kind": "repeat", "cadence": "quarterly", "start": "2026-11-30"}), 3),
        vec!["2026-11-30", "2027-02-28", "2027-05-30"]
    );
    // XP period of an occurrence runs from the previous occurrence to its date
    let o = occurrence(&sched(monthly("2026-01-31")), 1).unwrap();
    assert_eq!((o.period_start, o.period_end), (date("2026-01-31").and_hms_opt(0, 0, 0).unwrap().and_utc(), o.occurs_at));
}

#[test]
fn end_date_and_occurrence_count_are_respected() {
    let counted = sched(json!({"kind": "repeat", "cadence": "weekly", "start": "2026-01-05", "count": 3}));
    let plan = due_occurrences(&counted, None, at(2027, 1, 1), &HashSet::new());
    assert_eq!(plan.due.len(), 3);
    assert!(plan.schedule_exhausted);
    let ended = sched(json!({"kind": "repeat", "cadence": "monthly", "start": "2026-01-15", "end": "2026-04-15"}));
    let plan = due_occurrences(&ended, None, at(2027, 1, 1), &HashSet::new());
    assert_eq!(plan.due.iter().map(|o| o.period_key.clone()).collect::<Vec<_>>(), vec!["2026-01-15", "2026-02-15", "2026-03-15", "2026-04-15"]);
    // the first limit reached wins
    let both = sched(json!({"kind": "repeat", "cadence": "monthly", "start": "2026-01-15", "end": "2026-12-15", "count": 2}));
    assert_eq!(due_occurrences(&both, None, at(2027, 1, 1), &HashSet::new()).due.len(), 2);
    assert!(!sched(json!({"kind": "repeat", "cadence": "monthly", "start": "2026-01-15", "end": "2025-01-01"})).validate().is_empty());
}

// ================================================================== schedules with the database

#[tokio::test]
async fn a_paused_order_skips_and_does_not_catch_up() {
    let pool = pool().await;
    store(&pool, &base_policy()).await;
    let alice = payee(&pool, "Alice", &["alice"], None).await;
    let o = order(&pool, &who("staff", &[]), fixed(&alice, 1000), monthly("2026-01-01")).await.unwrap();
    approve(&pool, &o.id, "owner1").await.unwrap();

    materialize(&pool, at(2026, 1, 1)).await;
    assert_eq!(source_rows(&pool).await.len(), 1);
    payorders::transition(&pool, &cfg(), &dir(), &who("staff", &[]), &o.id, Transition::Pause, at(2026, 1, 15)).await.unwrap();
    let r = materialize(&pool, at(2026, 2, 1)).await;
    assert!(r.materialized.is_empty(), "paused: nothing");
    payorders::transition(&pool, &cfg(), &dir(), &who("staff", &[]), &o.id, Transition::Resume, at(2026, 2, 10)).await.unwrap();
    let r = materialize(&pool, at(2026, 2, 11)).await;
    assert!(r.materialized.is_empty(), "February fell while paused and is never paid: {r:?}");
    assert_eq!(r.skipped, vec![(o.id.clone(), "2026-02-01".to_string())]);
    let r = materialize(&pool, at(2026, 3, 1)).await;
    assert_eq!(r.materialized.len(), 1);
    let keys: Vec<String> = source_rows(&pool).await.iter().map(|e| e.title.clone()).collect();
    assert_eq!(keys, vec!["Order (2026-01-01)", "Order (2026-03-01)"]);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM pay_order_occurrences WHERE status = 'skipped'").await, 1);
}

#[tokio::test]
async fn the_same_occurrence_is_never_materialized_twice_even_across_scheduler_ticks() {
    let pool = pool().await;
    store(&pool, &base_policy()).await;
    let alice = payee(&pool, "Alice", &["alice"], None).await;
    let bob = payee(&pool, "Bob", &["bob"], None).await;
    let o = order(
        &pool,
        &who("staff", &[]),
        json!({"kind": "pool_percent", "pool_cents": 1000, "shares": [{"payee_id": alice, "bps": 6000}, {"payee_id": bob, "bps": 4000}]}),
        json!({"kind": "repeat", "cadence": "weekly", "start": "2026-03-02"}),
    )
    .await
    .unwrap();
    approve(&pool, &o.id, "owner1").await.unwrap();
    let stripe = Arc::new(MockStripe::default());
    let c = ctx(&pool, stripe.clone());
    let now = at(2026, 3, 10); // two occurrences due: 03-02 and 03-09
    for _ in 0..3 {
        schedule::scheduler_tick(&c, now).await.unwrap();
        payorders::materialize_due(&c, now).await.unwrap();
    }
    let rows = source_rows(&pool).await;
    assert_eq!(rows.len(), 4, "2 occurrences x 2 payees, once each");
    assert_eq!(rows.iter().map(|r| r.amount_cents).sum::<i64>(), 2000);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM pay_order_occurrences").await, 2);
    assert_eq!(transfers(&stripe), 0, "materializing never transfers");
    // and the table itself refuses a second row for the same (order, period)
    let dup = sqlx::query(
        "INSERT INTO pay_order_occurrences (id, order_id, period_key, occurs_at, period_start, period_end, status, order_revision,
         pool_cents, paid_cents, leftover_cents, detail, created_at) VALUES ('x', ?, '2026-03-02', '', '', '', 'materialized', 1, 0, 0, 0, '{}', '')",
    )
    .bind(&o.id)
    .execute(&pool)
    .await;
    assert!(dup.is_err());
}

#[tokio::test]
async fn pool_by_xp_reads_xp_earned_inside_the_period_with_filters() {
    let pool = pool().await;
    store(&pool, &base_policy()).await;
    for (gh, id) in [("alice", "00000000-0000-0000-0000-00000000000a"), ("bob", "00000000-0000-0000-0000-00000000000b"), ("carol", "00000000-0000-0000-0000-00000000000c")] {
        sqlx::query("INSERT INTO contributors (id, github_username, created_at, updated_at) VALUES (?, ?, datetime('now'), datetime('now'))")
            .bind(id).bind(gh).execute(&pool).await.unwrap();
    }
    let act = |cid: &'static str, pts: i64, at: &'static str, repo: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query("INSERT INTO merit_activities (id, contributor_id, activity_type, merit_change, description, created_at, provider, host, repo)
                         VALUES (?, ?, 1, ?, 'x', ?, 'github', 'github.com', ?)")
                .bind(uuid::Uuid::new_v4().to_string()).bind(cid).bind(pts).bind(at).bind(repo).execute(&pool).await.unwrap();
        }
    };
    // February period: [2026-02-01, 2026-03-01). Both stored time formats occur in real data.
    act("00000000-0000-0000-0000-00000000000a", 30, "2026-02-10T08:00:00.123456+00:00", "org/alpha").await;
    act("00000000-0000-0000-0000-00000000000b", 10, "2026-02-20 10:00:00", "org/alpha").await;
    act("00000000-0000-0000-0000-00000000000b", 500, "2026-03-01 00:00:00", "org/alpha").await; // next period
    act("00000000-0000-0000-0000-00000000000a", 999, "2026-01-31 23:59:59", "org/alpha").await; // previous period
    act("00000000-0000-0000-0000-00000000000c", 40, "2026-02-15 10:00:00", "org/other").await; // filtered out by repo
    let alice = payee(&pool, "Alice", &["alice"], None).await;
    let bob = payee(&pool, "Bob", &["bob"], None).await;
    let _carol = payee(&pool, "Carol", &["carol"], None).await;
    let o = order(
        &pool,
        &who("staff", &[]),
        json!({"kind": "pool_by_xp", "pool_cents": 10_000, "repos": [{"provider": "github", "repo": "org/alpha"}]}),
        json!({"kind": "repeat", "cadence": "monthly", "start": "2026-02-01"}),
    )
    .await
    .unwrap();
    approve(&pool, &o.id, "owner1").await.unwrap();
    materialize(&pool, at(2026, 3, 1)).await;
    let rows = source_rows(&pool).await;
    let by: Vec<(String, i64)> = rows.iter().filter(|r| r.title.contains("2026-03-01")).map(|r| (r.payee_id.clone().unwrap(), r.amount_cents)).collect();
    let mut expect = vec![(alice.clone(), 7500), (bob.clone(), 2500)];
    expect.sort();
    let mut by_sorted = by.clone();
    by_sorted.sort();
    assert_eq!(by_sorted, expect, "Feb XP only, org/alpha only");

    // Zero qualifying XP pays nothing and records why.
    let o2 = order(
        &pool,
        &who("staff", &[]),
        json!({"kind": "pool_by_xp", "pool_cents": 10_000, "repos": [{"provider": "github", "repo": "org/none"}]}),
        once("2026-03-01"),
    )
    .await
    .unwrap();
    approve(&pool, &o2.id, "owner1").await.unwrap();
    let before = source_rows(&pool).await.len();
    materialize(&pool, at(2026, 3, 2)).await;
    assert_eq!(source_rows(&pool).await.len(), before);
    let detail: String = sqlx::query_scalar("SELECT detail FROM pay_order_occurrences WHERE order_id = ?").bind(&o2.id).fetch_one(&pool).await.unwrap();
    assert!(detail.contains("nothing is paid"), "{detail}");
}

#[tokio::test]
async fn preview_shows_the_next_occurrences_and_persists_nothing() {
    let pool = pool().await;
    store(&pool, &base_policy()).await;
    let alice = payee(&pool, "Alice", &["alice"], None).await;
    let o = order(&pool, &who("staff", &[]), fixed(&alice, 1000), monthly("2026-01-01")).await.unwrap();
    let p = payorders::preview(&pool, &cfg(), &dir(), &who("reader", &[]), &o.id, 3, at(2026, 1, 15)).await.unwrap();
    assert_eq!(p.items.iter().map(|i| i.period_key.as_str()).collect::<Vec<_>>(), vec!["2026-01-01", "2026-02-01", "2026-03-01"]);
    assert_eq!(p.items[0].timing, "due");
    assert!(p.items[0].waiting_for.iter().any(|w| w.contains("approvals")));
    assert_eq!(p.items[1].split.as_ref().unwrap().paid_cents, 1000);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM pay_order_occurrences").await, 0);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM payout_ledger").await, 0);
}

// ================================================================== authority

#[tokio::test]
async fn the_creator_and_the_editor_cannot_approve_their_own_order() {
    let pool = pool().await;
    store(&pool, &base_policy()).await;
    let alice = payee(&pool, "Alice", &["alice"], None).await;
    let o = order(&pool, &who("owner1", &[]), fixed(&alice, 1000), once("2026-12-01")).await.unwrap();
    assert!(matches!(approve(&pool, &o.id, "owner1").await, Err(PayoutError::Forbidden(_))), "creator");
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM pay_order_approvals").await, 0);
    // owner2 edits the amount: owner2 is now the author of the revision and cannot approve it
    let patch: payorders::OrderPatch = serde_json::from_value(json!({"rule": fixed(&alice, 2000)})).unwrap();
    payorders::update(&pool, &cfg(), &dir(), &who("owner2", &[]), &o.id, patch, Utc::now()).await.unwrap();
    assert!(matches!(approve(&pool, &o.id, "owner2").await, Err(PayoutError::Forbidden(_))), "editor");
    assert!(matches!(approve(&pool, &o.id, "owner1").await, Err(PayoutError::Forbidden(_))), "creator still");
    // control: a third member can
    let r = approve(&pool, &o.id, "owner3").await.unwrap();
    assert!(r.order.approved);
}

#[tokio::test]
async fn an_unapproved_order_materializes_nothing() {
    let pool = pool().await;
    let mut two = base_policy();
    two.approval.approvals_required = 2;
    store(&pool, &two).await;
    let alice = payee(&pool, "Alice", &["alice"], None).await;
    let o = order(&pool, &who("staff", &[]), fixed(&alice, 1000), once("2026-12-01")).await.unwrap();
    let r = materialize(&pool, at(2026, 12, 2)).await;
    assert!(r.materialized.is_empty());
    assert_eq!(source_rows(&pool).await.len(), 0);
    approve(&pool, &o.id, "owner1").await.unwrap();
    assert!(materialize(&pool, at(2026, 12, 2)).await.materialized.is_empty(), "1 of 2 approvals");
    // the same member approving twice counts once
    approve(&pool, &o.id, "owner1").await.unwrap();
    assert!(materialize(&pool, at(2026, 12, 2)).await.materialized.is_empty());
    approve(&pool, &o.id, "owner2").await.unwrap();
    assert_eq!(materialize(&pool, at(2026, 12, 2)).await.materialized.len(), 1);
    let rows = source_rows(&pool).await;
    assert_eq!(rows.len(), 1);
    // the order's approvals carried over to the row
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payout_approvals WHERE ledger_id = ?").bind(&rows[0].id).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 2);
    // a draft never materializes
    let d: payorders::NewOrder = serde_json::from_value(json!({"name": "D", "rule": fixed(&alice, 5), "schedule": once("2026-12-01"), "draft": true})).unwrap();
    let d = payorders::create(&pool, &cfg(), &dir(), &who("staff", &[]), d, Utc::now()).await.unwrap();
    approve(&pool, &d.id, "owner1").await.unwrap();
    approve(&pool, &d.id, "owner2").await.unwrap();
    assert!(materialize(&pool, at(2026, 12, 3)).await.materialized.is_empty());
}

#[tokio::test]
async fn editing_an_approved_order_clears_its_approvals() {
    let pool = pool().await;
    store(&pool, &base_policy()).await;
    let alice = payee(&pool, "Alice", &["alice"], None).await;
    let o = order(&pool, &who("staff", &[]), fixed(&alice, 1000), once("2026-12-01")).await.unwrap();
    assert!(approve(&pool, &o.id, "owner1").await.unwrap().order.approved);
    // A name change is not material: approvals stay.
    let v = payorders::update(&pool, &cfg(), &dir(), &who("staff", &[]), &o.id, serde_json::from_value(json!({"name": "Renamed"})).unwrap(), Utc::now()).await.unwrap();
    assert!(v.approved);
    for patch in [json!({"rule": fixed(&alice, 999_999)}), json!({"schedule": once("2026-11-01")})] {
        let v = payorders::update(&pool, &cfg(), &dir(), &who("staff", &[]), &o.id, serde_json::from_value(patch).unwrap(), Utc::now()).await.unwrap();
        assert!(!v.approved, "a material change clears approvals");
        assert_eq!(v.approvals_counted, 0);
        assert!(materialize(&pool, at(2026, 12, 2)).await.materialized.is_empty());
        approve(&pool, &o.id, "owner1").await.unwrap();
        assert!(payorders::get(&pool, &cfg(), &dir(), &who("r", &[]), &o.id).await.unwrap().approved);
        // revert to unapproved for the next round
        let _ = payorders::update(&pool, &cfg(), &dir(), &who("staff", &[]), &o.id, serde_json::from_value(json!({"rule": fixed(&alice, 1000)})).unwrap(), Utc::now()).await.unwrap();
    }
    assert_eq!(source_rows(&pool).await.len(), 0);
}

#[tokio::test]
async fn a_payee_cannot_create_edit_approve_or_pause_what_pays_them() {
    let pool = pool().await;
    store(&pool, &base_policy()).await;
    // Alice is linked by member id; Bob by payee key on a token; Carol by a key on another token.
    let alice = payee(&pool, "Alice", &["alice-gh"], Some("alice")).await;
    let bob = payee(&pool, "Bob", &["bob-gh"], None).await;
    let carol = payee(&pool, "Carol", &["carol-gh"], None).await;
    create_token(&pool, &dir(), TokenCreator::Bootstrap, NewToken {
        label: "carol old".into(), member_id: "carol".into(), scopes: vec![scopes::MERIT_READ.into()],
        payee_keys: vec!["carol-gh".into()], expires_at: None,
    }, Utc::now()).await.unwrap();
    for (member, keys, pid) in [("alice", vec![], &alice), ("bob", vec!["bob-gh"], &bob), ("carol", vec![], &carol)] {
        let me = who(member, &keys);
        assert!(matches!(order(&pool, &me, fixed(pid, 1000), once("2026-12-01")).await, Err(PayoutError::Forbidden(_))), "{member} create");
        // a pool that includes them
        let pool_rule = json!({"kind": "pool_percent", "pool_cents": 100, "shares": [{"payee_id": pid, "bps": 100}]});
        assert!(matches!(order(&pool, &me, pool_rule, once("2026-12-01")).await, Err(PayoutError::Forbidden(_))), "{member} pool");
        // an XP pool open to anyone could pay them
        let open = json!({"kind": "pool_by_xp", "pool_cents": 100});
        assert!(matches!(order(&pool, &me, open, once("2026-12-01")).await, Err(PayoutError::Forbidden(_))), "{member} open pool");

        let o = order(&pool, &who("staff", &[]), fixed(pid, 1000), once("2026-12-01")).await.unwrap();
        let patch: payorders::OrderPatch = serde_json::from_value(json!({"name": "mine"})).unwrap();
        assert!(matches!(payorders::update(&pool, &cfg(), &dir(), &me, &o.id, patch, Utc::now()).await, Err(PayoutError::Forbidden(_))), "{member} edit");
        assert!(matches!(payorders::approve(&pool, &cfg(), &dir(), &me, &o.id, Utc::now()).await, Err(PayoutError::Forbidden(_))), "{member} approve");
        for t in [Transition::Pause, Transition::Cancel] {
            assert!(matches!(payorders::transition(&pool, &cfg(), &dir(), &me, &o.id, t, Utc::now()).await, Err(PayoutError::Forbidden(_))), "{member} {t:?}");
        }
        // nor edit an order of someone else into one that pays them
        let other = order(&pool, &who("staff", &[]), fixed(&payee(&pool, &format!("X{member}"), &[], None).await, 10), once("2026-12-01")).await.unwrap();
        let patch: payorders::OrderPatch = serde_json::from_value(json!({"rule": fixed(pid, 10)})).unwrap();
        assert!(matches!(payorders::update(&pool, &cfg(), &dir(), &me, &other.id, patch, Utc::now()).await, Err(PayoutError::Forbidden(_))), "{member} redirect");
        // nor edit their own payee record
        assert!(matches!(
            payees::update(&pool, &dir(), &me, pid, serde_json::from_value(json!({"status": "active"})).unwrap(), Utc::now()).await,
            Err(PayoutError::Forbidden(_))
        ), "{member} payee record");
    }
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM pay_order_approvals").await, 0);
}

#[tokio::test]
async fn an_approver_below_the_minimum_rank_does_not_count() {
    let pool = pool().await;
    let mut ranked = base_policy();
    ranked.approval.min_approver_rank = 500;
    store(&pool, &ranked).await;
    let alice = payee(&pool, "Alice", &["alice"], None).await;
    let o = order(&pool, &who("staff", &[]), fixed(&alice, 1000), once("2026-12-01")).await.unwrap();
    let r = approve(&pool, &o.id, "dave").await.unwrap(); // no rank
    assert_eq!(r.result, "recorded");
    assert!(!r.order.approved);
    assert!(!r.order.approvals[0].counts);
    assert!(materialize(&pool, at(2026, 12, 2)).await.materialized.is_empty());
    assert!(approve(&pool, &o.id, "owner1").await.unwrap().order.approved, "rank 1000 counts");
    assert_eq!(materialize(&pool, at(2026, 12, 2)).await.materialized.len(), 1);
}

// ================================================================== invoices

fn inv(payee: &str, number: &str, lines: Value, tax: i64, total: i64) -> invoices::NewInvoice {
    serde_json::from_value(json!({"payee_id": payee, "number": number, "issue_date": "2026-11-01", "due_date": "2026-11-30",
        "lines": lines, "tax_cents": tax, "total_cents": total}))
    .unwrap()
}

fn two_lines() -> Value {
    json!([{"description": "Design work", "quantity": 10, "unit_amount_cents": 150}, {"description": "Review", "quantity": 1, "unit_amount_cents": 500}])
}

#[test]
fn invoice_total_must_equal_lines_plus_tax() {
    let lines: Vec<InvoiceLine> = serde_json::from_value(two_lines()).unwrap();
    assert_eq!(check_total(&lines, 100, 2100).unwrap(), 2000);
    assert!(check_total(&lines, 100, 2099).is_err());
    assert!(check_total(&lines, 0, 2100).is_err());
    assert!(check_total(&[], 0, 0).is_err());
}

#[tokio::test]
async fn an_invoice_whose_total_does_not_match_is_refused() {
    let pool = pool().await;
    let alice = payee(&pool, "Alice", &["alice"], None).await;
    let r = invoices::submit(&pool, &cfg(), &dir(), &who("staff", &[]), inv(&alice, "A-1", two_lines(), 100, 2000), Utc::now()).await;
    assert!(matches!(r, Err(PayoutError::Invalid(ref e)) if e.iter().any(|m| m.contains("does not equal"))), "{r:?}");
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM invoices").await, 0);
}

#[tokio::test]
async fn a_duplicate_invoice_number_for_the_same_payee_is_refused() {
    let pool = pool().await;
    let alice = payee(&pool, "Alice", &["alice"], None).await;
    let bob = payee(&pool, "Bob", &["bob"], None).await;
    invoices::submit(&pool, &cfg(), &dir(), &who("staff", &[]), inv(&alice, "INV-7", two_lines(), 0, 2000), Utc::now()).await.unwrap();
    for n in ["INV-7", "inv-7", " INV-7 "] {
        let r = invoices::submit(&pool, &cfg(), &dir(), &who("staff", &[]), inv(&alice, n, two_lines(), 0, 2000), Utc::now()).await;
        assert!(matches!(r, Err(PayoutError::Invalid(_))), "{n}: {r:?}");
    }
    // another payee may use the same number
    invoices::submit(&pool, &cfg(), &dir(), &who("staff", &[]), inv(&bob, "INV-7", two_lines(), 0, 2000), Utc::now()).await.unwrap();
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM invoices").await, 2);
}

#[tokio::test]
async fn an_approved_invoice_is_one_ledger_row_and_a_paid_invoice_is_never_paid_again() {
    let pool = pool().await;
    store(&pool, &base_policy()).await;
    let alice = payee(&pool, "Alice", &["alice"], Some("alice")).await;
    let stripe = MockStripe::default();
    // Alice submits her own invoice (the one thing a payee may do for themselves).
    let i = invoices::submit(&pool, &cfg(), &dir(), &who("alice", &[]), inv(&alice, "A-1", two_lines(), 0, 2000), Utc::now()).await.unwrap();
    let r = invoices::approve(&pool, &cfg(), &dir(), &who("owner1", &[]), &i.id, Utc::now()).await.unwrap();
    assert_eq!(r.invoice.status, "scheduled");
    let lid = r.invoice.ledger_id.clone().unwrap();
    let row = ledger::get(&pool, &lid).await.unwrap().unwrap();
    assert_eq!((row.source_kind.as_str(), row.amount_cents, row.pay_after.as_deref()), (SOURCE_INVOICE, 2000, Some("2026-11-30 00:00:00")));

    // not payable before the due date
    let early = pay_run(&pool, &stripe, at(2026, 11, 29)).await;
    assert!(early.items.is_empty() || early.items.iter().all(|i| i.outcome != "paid"));
    assert_eq!(transfers(&stripe), 0);
    let paid = pay_run(&pool, &stripe, at(2026, 11, 30)).await;
    assert_eq!(paid.paid_cents, 2000);
    assert_eq!(transfers(&stripe), 1);
    let v = invoices::get(&pool, &cfg(), &dir(), &who("r", &[]), &i.id, Utc::now()).await.unwrap();
    assert_eq!(v.status, "paid");
    let actions: Vec<String> = v.events.iter().map(|e| format!("{}:{}", e.action, e.actor)).collect();
    assert_eq!(actions, vec!["submit:alice", "approval:owner1", "approve:owner1", "schedule:owner1", "paid:admin-token"]);

    // never again: approve, void, another run
    assert!(matches!(invoices::approve(&pool, &cfg(), &dir(), &who("owner2", &[]), &i.id, Utc::now()).await, Err(PayoutError::InvalidState(_))));
    assert!(matches!(invoices::void(&pool, &cfg(), &dir(), &who("owner2", &[]), &i.id, None, Utc::now()).await, Err(PayoutError::InvalidState(_))));
    pay_run(&pool, &stripe, at(2026, 12, 5)).await;
    assert_eq!(transfers(&stripe), 1);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM payout_ledger WHERE source_kind = 'invoice'").await, 1);
}

#[tokio::test]
async fn a_rejected_or_void_invoice_is_never_paid() {
    let pool = pool().await;
    store(&pool, &base_policy()).await;
    let alice = payee(&pool, "Alice", &["alice"], None).await;
    let stripe = MockStripe::default();
    let rej = invoices::submit(&pool, &cfg(), &dir(), &who("staff", &[]), inv(&alice, "R-1", two_lines(), 0, 2000), Utc::now()).await.unwrap();
    invoices::reject(&pool, &cfg(), &dir(), &who("owner1", &[]), &rej.id, Some("wrong rate"), Utc::now()).await.unwrap();
    assert!(matches!(invoices::approve(&pool, &cfg(), &dir(), &who("owner2", &[]), &rej.id, Utc::now()).await, Err(PayoutError::InvalidState(_))));

    let v = invoices::submit(&pool, &cfg(), &dir(), &who("staff", &[]), inv(&alice, "V-1", two_lines(), 0, 2000), Utc::now()).await.unwrap();
    let a = invoices::approve(&pool, &cfg(), &dir(), &who("owner1", &[]), &v.id, Utc::now()).await.unwrap();
    let lid = a.invoice.ledger_id.unwrap();
    let voided = invoices::void(&pool, &cfg(), &dir(), &who("owner2", &[]), &v.id, Some("duplicate"), Utc::now()).await.unwrap();
    assert_eq!(voided.status, "void");
    assert_eq!(ledger::get(&pool, &lid).await.unwrap().unwrap().status, REJECTED);

    pay_run(&pool, &stripe, at(2027, 1, 1)).await;
    assert_eq!(transfers(&stripe), 0);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM payout_ledger WHERE status = 'paid'").await, 0);
}

#[tokio::test]
async fn the_payee_and_the_submitter_cannot_approve_the_invoice() {
    let pool = pool().await;
    store(&pool, &base_policy()).await;
    let alice = payee(&pool, "Alice", &["alice-gh"], Some("alice")).await;
    let own = invoices::submit(&pool, &cfg(), &dir(), &who("alice", &[]), inv(&alice, "A-1", two_lines(), 0, 2000), Utc::now()).await.unwrap();
    for me in [who("alice", &[]), who("alice2", &["alice-gh"])] {
        let r = invoices::approve(&pool, &cfg(), &dir(), &me, &own.id, Utc::now()).await;
        assert!(matches!(r, Err(PayoutError::Forbidden(_))), "{}: {r:?}", me.member_id);
        assert!(matches!(invoices::reject(&pool, &cfg(), &dir(), &me, &own.id, None, Utc::now()).await, Err(PayoutError::Forbidden(_))));
    }
    let staff = invoices::submit(&pool, &cfg(), &dir(), &who("owner1", &[]), inv(&alice, "A-2", two_lines(), 0, 2000), Utc::now()).await.unwrap();
    assert!(matches!(invoices::approve(&pool, &cfg(), &dir(), &who("owner1", &[]), &staff.id, Utc::now()).await, Err(PayoutError::Forbidden(_))), "submitter");
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM invoice_approvals").await, 0);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM payout_ledger").await, 0);
}

// ================================================================== ledger

/// One approved pay order row for `payee_id`, due on `day` (materialized on that day).
async fn order_row(pool: &SqlitePool, payee_id: &str, cents: i64, day: &str) -> ledger::LedgerEntry {
    let o = order(pool, &who("staff", &[]), fixed(payee_id, cents), once(day)).await.unwrap();
    approve(pool, &o.id, "owner1").await.unwrap();
    let d = date(day);
    materialize(pool, Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap())).await;
    let id: String = sqlx::query_scalar(
        "SELECT l.id FROM payout_ledger l JOIN pay_order_occurrences o ON o.id = l.source_ref WHERE o.order_id = ?",
    )
    .bind(&o.id)
    .fetch_one(pool)
    .await
    .unwrap();
    ledger::get(pool, &id).await.unwrap().unwrap()
}

/// The claim with no limits, at noon on 2026-12-01 (the test clock).
async fn claim_on_day(pool: &SqlitePool, stripe: &MockStripe, id: &str) -> Result<ledger::LedgerEntry, LedgerError> {
    let g = ledger::ClaimGuard { now: Some("2026-12-01 12:00:00".into()), ..Default::default() };
    ledger::pay_guarded(pool, stripe, id, "t", &g).await
}

async fn blockers(pool: &SqlitePool, p: &PayoutPolicy, e: &ledger::LedgerEntry, now: DateTime<Utc>) -> Vec<Blocker> {
    let facts = approvals::load_facts(pool, &dir(), e.clone()).await.unwrap();
    approvals::evaluate(p, &facts, now, &approvals::Usage::default())
}

#[tokio::test]
async fn non_merit_rows_obey_pay_after_hold_limits_blocks_and_the_kill_switch() {
    let pool = pool().await;
    store(&pool, &base_policy()).await;
    let alice = payee(&pool, "Alice", &["alice", "alice-second"], None).await;
    let stripe = MockStripe::default();
    let e = order_row(&pool, &alice, 3000, "2026-12-01").await;
    assert_eq!(e.github_username, "alice", "the payee's first key");
    assert_eq!(e.source_kind, SOURCE_PAY_ORDER);
    assert!(e.pr_id < 0 && e.merge_sha.contains(':'), "legacy columns never look like a merge");

    let p = base_policy();
    let day = at(2026, 12, 1);
    assert_eq!(blockers(&pool, &p, &e, day).await, vec![], "payable on the day");
    // pay_after: not before the occurrence date, in evaluate and in the claim (guarded or not)
    let eve = Utc.with_ymd_and_hms(2026, 11, 30, 23, 0, 0).unwrap();
    assert!(blockers(&pool, &p, &e, eve).await.iter().any(|b| matches!(b, Blocker::NotYet { .. })));
    let g = approvals::claim_guard(&p, eve, None);
    assert!(matches!(ledger::pay_guarded(&pool, &stripe, &e.id, "t", &g).await, Err(LedgerError::NotPayable(_))));
    sqlx::query("UPDATE payout_ledger SET pay_after = '2099-01-01 00:00:00' WHERE id = ?").bind(&e.id).execute(&pool).await.unwrap();
    assert!(matches!(ledger::approve(&pool, &stripe, &e.id, "t").await, Err(LedgerError::NotPayable(_))), "unguarded path too");
    sqlx::query("UPDATE payout_ledger SET pay_after = '2026-12-01 00:00:00' WHERE id = ?").bind(&e.id).execute(&pool).await.unwrap();

    // hold
    let mut hold = p.clone();
    hold.limits.hold_days = Some(3);
    assert!(blockers(&pool, &hold, &e, day).await.iter().any(|b| matches!(b, Blocker::Hold { .. })));
    // limits
    let mut lim = p.clone();
    lim.limits.max_per_payout_cents = Some(2999);
    assert!(blockers(&pool, &lim, &e, day).await.iter().any(|b| matches!(b, Blocker::AboveMaxPerPayout { .. })));
    let g = approvals::claim_guard(&lim, day, None);
    assert!(matches!(ledger::pay_guarded(&pool, &stripe, &e.id, "t", &g).await, Err(LedgerError::NotPayable(_))));
    // kill switch
    let mut off = p.clone();
    off.enabled = false;
    assert!(blockers(&pool, &off, &e, day).await.contains(&Blocker::PolicyDisabled));
    // a block on ANY of the payee's keys blocks its pay order rows
    approvals::block_payee(&pool, &Principal::legacy_admin(), "alice-second", "investigation").await.unwrap();
    assert!(blockers(&pool, &p, &e, day).await.contains(&Blocker::PayeeBlocked));
    let g = approvals::claim_guard(&p, day, None);
    assert!(matches!(ledger::pay_guarded(&pool, &stripe, &e.id, "t", &g).await, Err(LedgerError::NotPayable(_))));
    assert_eq!(transfers(&stripe), 0);
    approvals::unblock_payee(&pool, &Principal::legacy_admin(), "alice-second").await.unwrap();

    // control: with none of these, the run pays it once, to the payee's own account
    let r = pay_run(&pool, &stripe, day).await;
    assert_eq!(r.paid_cents, 3000);
    let t = stripe.transfers.lock().unwrap().clone();
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].destination, "acct_alice");
    assert_eq!(t[0].idempotency_key, format!("dsync-ledger-{}", e.id));
}

#[tokio::test]
async fn a_paused_or_closed_payee_is_not_paid() {
    let pool = pool().await;
    store(&pool, &base_policy()).await;
    let alice = payee(&pool, "Alice", &["alice"], None).await;
    let stripe = MockStripe::default();
    let e = order_row(&pool, &alice, 1000, "2026-12-01").await;
    // and a merit row paid under the same key
    add_contributor(&pool, "alice", Some("acct_alice_gh"), true).await;
    let m = ledger::record_merge(&pool, &cfg(), &super::tests::merge(77, "abc77", "main")).await.unwrap().unwrap();
    assert_eq!(m.source_kind, SOURCE_MERIT);
    for status in ["paused", "closed"] {
        sqlx::query("UPDATE payees SET status = ? WHERE id = ?").bind(status).bind(&alice).execute(&pool).await.unwrap();
        let b = blockers(&pool, &base_policy(), &e, at(2026, 12, 1)).await;
        assert!(b.iter().any(|b| matches!(b, Blocker::PayeeInactive { .. })), "{status}: {b:?}");
        assert!(matches!(claim_on_day(&pool, &stripe, &e.id).await, Err(LedgerError::NotPayable(_))), "{status} pay order row");
        assert!(matches!(ledger::approve(&pool, &stripe, &m.id, "t").await, Err(LedgerError::NotPayable(_))), "{status} merit row");
        pay_run(&pool, &stripe, at(2026, 12, 1)).await;
        assert_eq!(transfers(&stripe), 0, "{status}");
    }
    // a payee whose Stripe account cannot receive transfers is not paid either
    sqlx::query("UPDATE payees SET status = 'active', stripe_payouts_enabled = 0 WHERE id = ?").bind(&alice).execute(&pool).await.unwrap();
    assert!(matches!(claim_on_day(&pool, &stripe, &e.id).await, Err(LedgerError::NotPayable(_))));
    // the Stripe webhook turns it on for the payee as for contributors
    ledger::update_account_status(&pool, "acct_alice", true).await.unwrap();
    assert_eq!(claim_on_day(&pool, &stripe, &e.id).await.unwrap().status, PAID);
    assert_eq!(transfers(&stripe), 1);
}

#[tokio::test]
async fn executing_a_run_twice_transfers_once_per_row() {
    let pool = pool().await;
    store(&pool, &base_policy()).await;
    let alice = payee(&pool, "Alice", &["alice"], None).await;
    let bob = payee(&pool, "Bob", &["bob"], None).await;
    order_row(&pool, &alice, 1000, "2026-12-01").await;
    order_row(&pool, &bob, 2000, "2026-12-01").await;
    let stripe = MockStripe::default();
    let id = match runs::create_run(&pool, &cfg(), &dir(), &Principal::legacy_admin(), false, at(2026, 12, 1)).await.unwrap() {
        CreateRunResult::Created { run, .. } => run.run.id,
        _ => unreachable!(),
    };
    for _ in 0..3 {
        runs::execute_run(&pool, &stripe, &cfg(), &dir(), &Principal::legacy_admin(), &id, at(2026, 12, 1)).await.unwrap();
    }
    assert_eq!(transfers(&stripe), 2);
    let keys: HashSet<String> = stripe.transfers.lock().unwrap().iter().map(|t| t.idempotency_key.clone()).collect();
    assert_eq!(keys.len(), 2);
}

#[tokio::test]
async fn source_rows_are_unique_and_never_collide_with_merges() {
    let pool = pool().await;
    add_contributor(&pool, "alice", Some("acct_1"), true).await;
    let merit = ledger::record_merge(&pool, &cfg(), &super::tests::merge(1, "deadbeef", "main")).await.unwrap().unwrap();
    assert_eq!((merit.source_kind.as_str(), merit.payee_id.as_deref(), merit.pay_after.as_deref()), (SOURCE_MERIT, None, None));
    let src = SourceRow {
        source_kind: SOURCE_INVOICE,
        source_ref: "inv-1".into(),
        payee_id: "p1".into(),
        payee_key: "alice".into(),
        contributor_id: None,
        title: "Invoice".into(),
        amount_cents: 100,
        currency: "usd".into(),
        pay_after: None,
        created_at: "2026-11-19 00:00:00".into(),
    };
    let mut conn = pool.acquire().await.unwrap();
    assert!(ledger::insert_source_row(&mut conn, &src).await.unwrap().is_some());
    assert!(ledger::insert_source_row(&mut conn, &src).await.unwrap().is_none(), "one row per (kind, ref, payee)");
    drop(conn);
    let (pr, sha) = ledger::legacy_columns(SOURCE_PAY_ORDER, "occ", "p1");
    assert!(pr < 0 && sha == "pay_order:occ:p1");
    // merit dedupe still holds exactly as before
    assert!(ledger::record_merge(&pool, &cfg(), &super::tests::merge(1, "deadbeef", "main")).await.unwrap().is_none());
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM payout_ledger").await, 2);
    assert_eq!(merit.status, PENDING);
}

#[tokio::test]
async fn statement_groups_paid_and_pending_by_source() {
    let pool = pool().await;
    store(&pool, &base_policy()).await;
    let alice = payee(&pool, "Alice", &["alice"], None).await;
    let stripe = MockStripe::default();
    order_row(&pool, &alice, 1000, "2026-12-01").await;
    pay_run(&pool, &stripe, at(2026, 12, 1)).await;
    let i = invoices::submit(&pool, &cfg(), &dir(), &who("staff", &[]), inv(&alice, "S-1", two_lines(), 0, 2000), Utc::now()).await.unwrap();
    invoices::approve(&pool, &cfg(), &dir(), &who("owner1", &[]), &i.id, Utc::now()).await.unwrap();
    add_contributor(&pool, "alice", Some("acct_gh"), true).await;
    ledger::record_merge(&pool, &cfg(), &super::tests::merge(5, "abc5", "main")).await.unwrap().unwrap();
    let s = statement::statement(&pool, &who("r", &[]), &alice, "2020-01-01", "2030-01-01").await.unwrap();
    let by: Vec<(String, i64, i64)> = s.sources.iter().map(|t| (t.source_kind.clone(), t.paid_cents, t.pending_cents)).collect();
    assert_eq!(by, vec![("invoice".into(), 0, 2000), ("merit".into(), 0, 3000), ("pay_order".into(), 1000, 0)]);
    assert_eq!((s.paid_cents, s.pending_cents), (1000, 5000));
    let none = statement::statement(&pool, &who("r", &[]), &alice, "2001-01-01", "2001-01-31").await.unwrap();
    assert!(none.sources.is_empty());
    assert!(statement::statement(&pool, &who("r", &[]), &alice, "2026-12-02", "2026-12-01").await.is_err());
}

#[tokio::test]
async fn payees_hold_no_bank_fields_and_new_accounts_start_disabled() {
    let pool = pool().await;
    let bad: Result<payees::NewPayee, _> = serde_json::from_value(json!({"display_name": "X", "kind": "company", "bank_account_number": "123"}));
    assert!(bad.is_err(), "there is no bank field");
    let p = payees::create(&pool, &dir(), &who("staff", &[]), serde_json::from_value(json!({
        "display_name": "Acme", "kind": "company", "stripe_account_id": "acct_acme"})).unwrap(), Utc::now()).await.unwrap();
    assert!(!p.row.stripe_payouts_enabled, "only the Stripe webhook enables payouts");
    ledger::update_account_status(&pool, "acct_acme", true).await.unwrap();
    assert!(payees::load(&pool, &p.row.id).await.unwrap().unwrap().row.stripe_payouts_enabled);
    // changing the account turns payouts off again
    let p2 = payees::update(&pool, &dir(), &who("staff", &[]), &p.row.id, serde_json::from_value(json!({"stripe_account_id": "acct_other"})).unwrap(), Utc::now()).await.unwrap();
    assert!(!p2.row.stripe_payouts_enabled);
    // a payee linked by key to an onboarded contributor takes its account
    add_contributor(&pool, "zed", Some("acct_zed"), true).await;
    let z = payees::create(&pool, &dir(), &who("staff", &[]), serde_json::from_value(json!({
        "display_name": "Zed", "kind": "individual", "payee_keys": ["Zed"]})).unwrap(), Utc::now()).await.unwrap();
    assert_eq!((z.row.stripe_account_id.as_deref(), z.row.stripe_payouts_enabled), (Some("acct_zed"), true));
    assert_eq!(z.payee_keys, vec!["zed"]);
    // two payees cannot share a key
    let dup = payees::create(&pool, &dir(), &who("staff", &[]), serde_json::from_value(json!({
        "display_name": "Zed2", "kind": "individual", "payee_keys": ["zed"]})).unwrap(), Utc::now()).await;
    assert!(matches!(dup, Err(PayoutError::Invalid(_))));
    let events = count(&pool, "SELECT COUNT(*) FROM payee_events").await;
    assert_eq!(events, 3);
}

// ================================================================== REST

macro_rules! app {
    ($ctx:expr) => {
        atest::init_service(App::new().app_data(web::Data::new($ctx)).configure(api::configure)).await
    };
}

async fn token(pool: &SqlitePool, member: &str, scopes: &[&str]) -> String {
    create_token(pool, &dir(), TokenCreator::Bootstrap, NewToken {
        label: format!("{member} t"), member_id: member.into(), scopes: scopes.iter().map(|s| s.to_string()).collect(),
        payee_keys: vec![], expires_at: None,
    }, Utc::now()).await.unwrap().token
}

#[actix_web::test]
async fn every_payroll_route_refuses_a_token_without_its_scope() {
    let pool = pool().await;
    store(&pool, &base_policy()).await;
    let alice = payee(&pool, "Alice", &["alice"], None).await;
    let o = order(&pool, &who("staff", &[]), fixed(&alice, 1000), once("2026-12-01")).await.unwrap();
    let i = invoices::submit(&pool, &cfg(), &dir(), &who("staff", &[]), inv(&alice, "Z-1", two_lines(), 0, 2000), Utc::now()).await.unwrap();
    let app = app!(ctx(&pool, Arc::new(MockStripe::default())));
    let new_payee = json!({"display_name": "N", "kind": "individual"});
    let new_order = json!({"name": "N", "rule": fixed(&alice, 5), "schedule": once("2026-12-01")});
    let new_inv = json!({"payee_id": alice, "number": "Z-2", "issue_date": "2026-11-01", "due_date": "2026-11-30",
        "lines": two_lines(), "total_cents": 2000});
    let routes: Vec<(&str, String, Option<Value>, &str)> = vec![
        ("GET", "/api/v1/payees".into(), None, scopes::PAYEES_READ),
        ("POST", "/api/v1/payees".into(), Some(new_payee), scopes::PAYEES_MANAGE),
        ("GET", format!("/api/v1/payees/{alice}"), None, scopes::PAYEES_READ),
        ("PATCH", format!("/api/v1/payees/{alice}"), Some(json!({"notes": "x"})), scopes::PAYEES_MANAGE),
        ("GET", "/api/v1/payorders".into(), None, scopes::PAYORDERS_READ),
        ("POST", "/api/v1/payorders".into(), Some(new_order), scopes::PAYORDERS_MANAGE),
        ("GET", format!("/api/v1/payorders/{}", o.id), None, scopes::PAYORDERS_READ),
        ("PATCH", format!("/api/v1/payorders/{}", o.id), Some(json!({"name": "x"})), scopes::PAYORDERS_MANAGE),
        ("POST", format!("/api/v1/payorders/{}/approve", o.id), None, scopes::PAYORDERS_APPROVE),
        ("POST", format!("/api/v1/payorders/{}/pause", o.id), None, scopes::PAYORDERS_MANAGE),
        ("POST", format!("/api/v1/payorders/{}/resume", o.id), None, scopes::PAYORDERS_MANAGE),
        ("POST", format!("/api/v1/payorders/{}/cancel", o.id), None, scopes::PAYORDERS_MANAGE),
        ("GET", format!("/api/v1/payorders/{}/preview?periods=2", o.id), None, scopes::PAYORDERS_READ),
        ("GET", "/api/v1/invoices".into(), None, scopes::INVOICES_READ),
        ("POST", "/api/v1/invoices".into(), Some(new_inv), scopes::INVOICES_SUBMIT),
        ("GET", format!("/api/v1/invoices/{}", i.id), None, scopes::INVOICES_READ),
        ("POST", format!("/api/v1/invoices/{}/approve", i.id), None, scopes::INVOICES_APPROVE),
        ("POST", format!("/api/v1/invoices/{}/reject", i.id), None, scopes::INVOICES_APPROVE),
        ("POST", format!("/api/v1/invoices/{}/void", i.id), None, scopes::INVOICES_APPROVE),
        ("GET", format!("/api/v1/payouts/statement?payee={alice}&from=2026-01-01&to=2026-12-31"), None, scopes::PAYOUTS_READ),
    ];
    let before = (
        count(&pool, "SELECT COUNT(*) FROM payees").await,
        count(&pool, "SELECT COUNT(*) FROM pay_orders").await,
        count(&pool, "SELECT COUNT(*) FROM invoices").await,
        count(&pool, "SELECT COUNT(*) FROM pay_order_events").await,
        count(&pool, "SELECT COUNT(*) FROM invoice_events").await,
    );
    for (method, uri, body, scope) in &routes {
        let others: Vec<&str> = scopes::ALL.iter().copied().filter(|s| s != scope).collect();
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
    let after = (
        count(&pool, "SELECT COUNT(*) FROM payees").await,
        count(&pool, "SELECT COUNT(*) FROM pay_orders").await,
        count(&pool, "SELECT COUNT(*) FROM invoices").await,
        count(&pool, "SELECT COUNT(*) FROM pay_order_events").await,
        count(&pool, "SELECT COUNT(*) FROM invoice_events").await,
    );
    assert_eq!(before, after, "nothing changed");
    // control: with every scope each GET succeeds
    let all = token(&pool, "owner2", scopes::ALL).await;
    for (method, uri, _, _) in routes.iter().filter(|r| r.0 == "GET") {
        let req = atest::TestRequest::get().uri(uri).insert_header(("Authorization", format!("Bearer {all}")));
        let resp = atest::call_service(&app, req.to_request()).await;
        assert_eq!(resp.status(), 200, "{method} {uri}");
    }
}

#[actix_web::test]
async fn request_bodies_cannot_name_an_actor() {
    let pool = pool().await;
    store(&pool, &base_policy()).await;
    let alice = payee(&pool, "Alice", &["alice"], None).await;
    let o = order(&pool, &who("staff", &[]), fixed(&alice, 1000), once("2026-12-01")).await.unwrap();
    let i = invoices::submit(&pool, &cfg(), &dir(), &who("staff", &[]), inv(&alice, "Q-1", two_lines(), 0, 2000), Utc::now()).await.unwrap();
    let app = app!(ctx(&pool, Arc::new(MockStripe::default())));
    let all = token(&pool, "owner2", scopes::ALL).await;
    let cases: Vec<(&str, String, Value)> = vec![
        ("POST", "/api/v1/payorders".into(), json!({"name": "N", "rule": fixed(&alice, 5), "schedule": once("2026-12-01"), "created_by": "mallory"})),
        ("PATCH", format!("/api/v1/payorders/{}", o.id), json!({"name": "x", "updated_by": "mallory"})),
        ("POST", format!("/api/v1/payorders/{}/approve", o.id), json!({"approver_member_id": "mallory"})),
        ("POST", format!("/api/v1/payorders/{}/pause", o.id), json!({"actor": "mallory"})),
        ("POST", "/api/v1/invoices".into(), json!({"payee_id": alice, "number": "Q-2", "issue_date": "2026-11-01", "due_date": "2026-11-30",
            "lines": two_lines(), "total_cents": 2000, "submitted_by": "mallory"})),
        ("POST", format!("/api/v1/invoices/{}/approve", i.id), json!({"approver_member_id": "mallory"})),
        ("POST", format!("/api/v1/invoices/{}/void", i.id), json!({"reason": "x", "actor": "mallory"})),
        ("POST", "/api/v1/payees".into(), json!({"display_name": "N", "kind": "individual", "created_by": "mallory"})),
        ("POST", "/api/v1/payees".into(), json!({"display_name": "N", "kind": "individual", "bank_account_number": "000123"})),
        ("PATCH", format!("/api/v1/payees/{alice}"), json!({"stripe_payouts_enabled": true})),
    ];
    for (method, uri, body) in &cases {
        let req = if *method == "POST" { atest::TestRequest::post() } else { atest::TestRequest::patch() }
            .uri(uri)
            .insert_header(("Authorization", format!("Bearer {all}")))
            .set_json(body);
        let resp = atest::call_service(&app, req.to_request()).await;
        assert_eq!(resp.status(), 400, "{method} {uri} {body}");
        let v: Value = atest::read_body_json(resp).await;
        assert!(v["message"].as_str().unwrap().contains("unknown field"), "{v}");
    }
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM pay_order_approvals").await, 0);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM invoice_approvals").await, 0);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM pay_orders").await, 1);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM payees").await, 1);

    // Control: without the extra field the actor is the token's member.
    let req = atest::TestRequest::post().uri(&format!("/api/v1/payorders/{}/approve", o.id)).insert_header(("Authorization", format!("Bearer {all}")));
    let resp = atest::call_service(&app, req.to_request()).await;
    assert_eq!(resp.status(), 200);
    let v: Value = atest::read_body_json(resp).await;
    assert_eq!(v["result"]["approver_member_id"], "owner2");
    let req = atest::TestRequest::post().uri("/api/v1/payorders").insert_header(("Authorization", format!("Bearer {all}")))
        .set_json(json!({"name": "N", "rule": fixed(&alice, 5), "schedule": once("2026-12-01")}));
    let resp = atest::call_service(&app, req.to_request()).await;
    assert_eq!(resp.status(), 201);
    let v: Value = atest::read_body_json(resp).await;
    assert_eq!(v["payorder"]["created_by"], "owner2");
}

#[tokio::test]
async fn a_rule_or_schedule_with_unknown_fields_is_refused() {
    assert!(serde_json::from_value::<AmountRule>(json!({"kind": "fixed", "payee_id": "p", "amount_cents": 5, "bonus": 1})).is_err());
    assert!(serde_json::from_value::<AmountRule>(json!({"kind": "split_evenly", "pool_cents": 5})).is_err());
    assert!(serde_json::from_value::<OrderSchedule>(json!({"kind": "once", "date": "2026-12-01", "every": "day"})).is_err());
    assert!(serde_json::from_value::<OrderSchedule>(json!({"kind": "repeat", "cadence": "daily", "start": "2026-12-01"})).is_err());
}
