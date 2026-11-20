//! Security tests for tokens-driven payout controls: approvals, policy, limits, runs, schedule,
//! and the legacy route. Stripe is the mock from `tests.rs`; nothing here calls live Stripe.

use super::api::{self, ApiContext};
use super::approvals::{self, counted_approvals, evaluate, ApproverFact, Blocker, LegacyApproveOutcome, RowApproval, RowFacts, Usage};
use super::error::PayoutError;
use super::ledger::{self, ClaimGuard, LedgerEntry, LedgerError, MergeEvent, PAID, PENDING};
use super::policy::{self, Cadence, PayoutPolicy, Schedule, ScheduleDay, ScheduleMode};
use super::runs::{self, CreateRunResult};
use super::schedule::{self, current_slot, run_due, TickOutcome};
use super::tests::{add_contributor, cfg, pool, MockStripe};
use crate::auth::{
    authenticate_token, create_token, scopes, AuthError, EnvDirectory, NewToken, Principal, TokenCreator,
};
use actix_web::{test as atest, web, App};
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::Arc;

const ALL_PAYOUT: &[&str] = &[
    scopes::PAYOUTS_READ,
    scopes::PAYOUTS_PREPARE,
    scopes::PAYOUTS_APPROVE,
    scopes::PAYOUTS_EXECUTE,
    scopes::PAYOUTS_POLICY,
];

fn dir() -> EnvDirectory {
    EnvDirectory::new(vec!["owner1".to_string(), "owner2".to_string()])
}

fn base_policy() -> PayoutPolicy {
    serde_json::from_value(json!({
        "schema": 1,
        "enabled": true,
        "currency": "usd",
        "schedule": {"cadence": "manual"},
        "limits": {},
        "approval": {"approvals_required": 1, "min_approver_rank": 0},
        "rate": [
            {"branch": "main", "base_cents": 2000, "merit_multiplier": 0.01},
            {"branch": "upcoming", "base_cents": 1000, "merit_multiplier": 0.01}
        ]
    }))
    .unwrap()
}

async fn store(pool: &SqlitePool, p: &PayoutPolicy) {
    policy::put_policy(pool, &Principal::legacy_admin(), p, None).await.unwrap();
}

async fn member(pool: &SqlitePool, id: &str, scopes: &[&str], payee: &[&str]) -> (Principal, String) {
    let t = create_token(
        pool,
        &dir(),
        TokenCreator::Bootstrap,
        NewToken {
            label: format!("{id} token"),
            member_id: id.into(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            payee_keys: payee.iter().map(|s| s.to_string()).collect(),
            expires_at: None,
        },
        Utc::now(),
    )
    .await
    .unwrap();
    (authenticate_token(pool, &t.token, Utc::now()).await.unwrap(), t.token)
}

fn merge_by(pr_id: i64, user: &str) -> MergeEvent {
    MergeEvent {
        pr_id,
        pr_number: pr_id,
        merge_sha: format!("sha{pr_id}"),
        repo: Some("org/eustress".into()),
        title: "Fix thing".into(),
        github_username: user.into(),
        branch: "main".into(),
        merit_points: 50, // 2000 * 1.5 = 3000 cents
    }
}

async fn row(pool: &SqlitePool, pr_id: i64, user: &str) -> LedgerEntry {
    ledger::record_merge(pool, &cfg(), &merge_by(pr_id, user)).await.unwrap().unwrap()
}

async fn prepare(pool: &SqlitePool, now: DateTime<Utc>) -> String {
    match runs::create_run(pool, &cfg(), &dir(), &Principal::legacy_admin(), false, now).await.unwrap() {
        CreateRunResult::Created { run, .. } => run.run.id,
        CreateRunResult::DryRun(_) => panic!("expected a persisted run"),
    }
}

async fn execute(pool: &SqlitePool, stripe: &MockStripe, run_id: &str, now: DateTime<Utc>) -> runs::ExecuteReport {
    runs::execute_run(pool, stripe, &cfg(), &dir(), &Principal::legacy_admin(), run_id, now).await.unwrap()
}

async fn status(pool: &SqlitePool, id: &str) -> String {
    ledger::get(pool, id).await.unwrap().unwrap().status
}

async fn plan_blockers(pool: &SqlitePool, now: DateTime<Utc>) -> Vec<(String, Vec<Blocker>)> {
    match runs::create_run(pool, &cfg(), &dir(), &Principal::legacy_admin(), true, now).await.unwrap() {
        CreateRunResult::DryRun(plan) => plan.items.into_iter().map(|i| (i.ledger_id, i.blockers)).collect(),
        CreateRunResult::Created { .. } => panic!("dry run persisted"),
    }
}

fn transfers(stripe: &MockStripe) -> usize {
    stripe.transfers.lock().unwrap().len()
}

// ------------------------------------------------------------------ policy

#[test]
fn default_policy_reproduces_env_amounts() {
    let c = cfg();
    let p = PayoutPolicy::from_config(&c);
    assert!(p.validate().is_empty());
    assert_eq!(p.approval.approvals_required, 1);
    assert_eq!(p.hold_days(), 0);
    for (branch, merit) in [("main", 50), ("main", 0), ("main", 1000), ("upcoming", 0), ("upcoming", 7), ("feature/x", 50)] {
        assert_eq!(p.amount_for(branch, merit), c.amount_for(branch, merit), "{branch} {merit}");
    }
}

#[test]
fn policy_validation_rejects_bad_values_and_unknown_keys() {
    assert!(base_policy().validate().is_empty());
    let mut p = base_policy();
    p.approval.approvals_required = 0;
    assert!(p.validate().iter().any(|e| e.contains("approvals_required")));

    let mut p = base_policy();
    p.limits.max_per_run_cents = Some(-1);
    assert!(p.validate().iter().any(|e| e.contains("max_per_run_cents")));

    let mut p = base_policy();
    p.schedule = Schedule { cadence: Cadence::Weekly, day: Some(ScheduleDay::Weekday("funday".into())), hour_utc: Some(17), mode: ScheduleMode::Prepare };
    assert!(p.validate().iter().any(|e| e.contains("weekday")));
    p.schedule.day = Some(ScheduleDay::Weekday("friday".into()));
    p.schedule.hour_utc = Some(24);
    assert!(p.validate().iter().any(|e| e.contains("hour_utc")));
    p.schedule = Schedule { cadence: Cadence::Monthly, day: Some(ScheduleDay::MonthDay(29)), hour_utc: Some(0), mode: ScheduleMode::Auto };
    assert!(p.validate().iter().any(|e| e.contains("1..=28")));

    // unknown key anywhere is an error (fail closed)
    let mut v = serde_json::to_value(base_policy()).unwrap();
    v["approval"]["self_approval"] = json!(true);
    assert!(serde_json::from_value::<PayoutPolicy>(v).is_err());

    // the contract 3.5 example (as JSON) parses and validates
    let contract: PayoutPolicy = serde_json::from_value(json!({
        "schema": 1, "enabled": true, "currency": "usd",
        "schedule": {"cadence": "weekly", "day": "friday", "hour_utc": 17, "mode": "prepare"},
        "limits": {"min_payout_cents": 500, "max_per_payout_cents": 10000, "max_per_member_per_period_cents": 50000,
                   "max_per_run_cents": 250000, "hold_days": 7},
        "approval": {"approvals_required": 2, "min_approver_rank": 60},
        "eligibility": {"min_rank": 0, "roles": [], "require_verified_identity": true},
        "rate": [{"branch": "main", "base_cents": 1000, "merit_multiplier": 0.01}]
    }))
    .unwrap();
    assert!(contract.validate().is_empty(), "{:?}", contract.validate());
}

#[tokio::test]
async fn policy_is_versioned_and_stored_rates_drive_new_rows() {
    let pool = pool().await;
    let eff = policy::load_effective(&pool, "default", &cfg()).await.unwrap();
    assert!(!eff.is_stored());

    let mut p = base_policy();
    p.rate = vec![policy::Rate { branch: "main".into(), base_cents: 100, merit_multiplier: 0.0 }];
    assert_eq!(policy::put_policy(&pool, &Principal::legacy_admin(), &p, Some(None)).await.unwrap(), 1);
    // stale expected version is refused
    assert!(matches!(
        policy::put_policy(&pool, &Principal::legacy_admin(), &p, Some(None)).await,
        Err(policy::PolicyError::Conflict(Some(1)))
    ));
    assert_eq!(policy::put_policy(&pool, &Principal::legacy_admin(), &p, Some(Some(1))).await.unwrap(), 2);
    let eff = policy::load_effective(&pool, "default", &cfg()).await.unwrap();
    assert_eq!(eff.version, Some(2));

    let e = row(&pool, 1, "alice").await;
    assert_eq!(e.amount_cents, 100, "stored policy rate, not env");
    // a branch the stored policy has no rate for records nothing
    let mut ev = merge_by(2, "alice");
    ev.branch = "upcoming".into();
    assert!(ledger::record_merge(&pool, &cfg(), &ev).await.unwrap().is_none());

    // writing needs payouts.policy; invalid policies are refused
    let (reader, _) = member(&pool, "bob", &[scopes::PAYOUTS_READ], &[]).await;
    assert!(matches!(policy::put_policy(&pool, &reader, &p, None).await, Err(policy::PolicyError::Auth(AuthError::MissingScope(_)))));
    let mut bad = base_policy();
    bad.approval.approvals_required = 0;
    assert!(matches!(policy::put_policy(&pool, &Principal::legacy_admin(), &bad, None).await, Err(policy::PolicyError::Invalid(_))));
}

// ------------------------------------------------------------------ approvals

#[tokio::test]
async fn payee_cannot_approve_own_row_directly() {
    let pool = pool().await;
    let e = row(&pool, 1, "Alice").await;
    let (alice, _) = member(&pool, "alice-m", &[scopes::PAYOUTS_APPROVE], &["alice"]).await;
    assert!(matches!(approvals::approve_row(&pool, &dir(), &alice, &e.id).await, Err(PayoutError::PayeeCannotApprove)));

    // A second token for the same member without payee_keys cannot hide it.
    let (alice2, _) = member(&pool, "alice-m", &[scopes::PAYOUTS_APPROVE], &[]).await;
    assert!(alice2.payee_keys.is_empty());
    assert!(matches!(approvals::approve_row(&pool, &dir(), &alice2, &e.id).await, Err(PayoutError::PayeeCannotApprove)));

    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payout_approvals").fetch_one(&pool).await.unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn payee_cannot_approve_own_row_through_a_run() {
    let pool = pool().await;
    let own = row(&pool, 1, "alice").await;
    let other = row(&pool, 2, "dave").await;
    let run_id = prepare(&pool, Utc::now()).await;
    let (alice, _) = member(&pool, "alice-m", &[scopes::PAYOUTS_APPROVE], &["alice"]).await;
    let report = runs::approve_run(&pool, &dir(), &alice, &run_id).await.unwrap();
    assert_eq!(report.skipped_payee, vec![own.id.clone()]);
    assert_eq!(report.approved, vec![other.id.clone()]);
    let mine: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payout_approvals WHERE ledger_id = ?")
        .bind(&own.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(mine, 0);
}

#[test]
fn a_payee_approval_never_counts() {
    let p = base_policy();
    let payee = ApproverFact { member_id: "alice-m".into(), rank: Some(1000), is_payee: true };
    let other = ApproverFact { member_id: "bob".into(), rank: None, is_payee: false };
    assert_eq!(counted_approvals(&p, &[payee.clone()]), 0);
    assert_eq!(counted_approvals(&p, &[payee, other]), 1);
}

#[tokio::test]
async fn one_member_approving_twice_counts_once() {
    let pool = pool().await;
    add_contributor(&pool, "alice", Some("acct_1"), true).await;
    let mut p = base_policy();
    p.approval.approvals_required = 2;
    store(&pool, &p).await;
    let e = row(&pool, 1, "alice").await;
    let (bob, _) = member(&pool, "bob", &[scopes::PAYOUTS_APPROVE], &[]).await;
    assert_eq!(approvals::approve_row(&pool, &dir(), &bob, &e.id).await.unwrap(), RowApproval::Recorded);
    assert_eq!(approvals::approve_row(&pool, &dir(), &bob, &e.id).await.unwrap(), RowApproval::AlreadyApproved);
    // The same member through another token is still the same approver.
    let (bob2, _) = member(&pool, "bob", &[scopes::PAYOUTS_APPROVE], &[]).await;
    assert_eq!(approvals::approve_row(&pool, &dir(), &bob2, &e.id).await.unwrap(), RowApproval::AlreadyApproved);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payout_approvals WHERE ledger_id = ?").bind(&e.id).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1);

    let stripe = MockStripe::default();
    let run_id = prepare(&pool, Utc::now()).await;
    let r = execute(&pool, &stripe, &run_id, Utc::now()).await;
    assert_eq!(r.items[0].outcome, "not_payable");
    assert_eq!(transfers(&stripe), 0);
}

#[test]
fn duplicate_approver_facts_count_once() {
    let p = base_policy();
    let bob = ApproverFact { member_id: "bob".into(), rank: None, is_payee: false };
    assert_eq!(counted_approvals(&p, &[bob.clone(), bob]), 1);
}

#[tokio::test]
async fn fewer_approvals_than_required_pays_nothing() {
    let pool = pool().await;
    add_contributor(&pool, "alice", Some("acct_1"), true).await;
    let mut p = base_policy();
    p.approval.approvals_required = 2;
    store(&pool, &p).await;
    let e = row(&pool, 1, "alice").await;
    let run_id = prepare(&pool, Utc::now()).await;
    let (bob, _) = member(&pool, "bob", &[scopes::PAYOUTS_APPROVE], &[]).await;
    runs::approve_run(&pool, &dir(), &bob, &run_id).await.unwrap();

    let stripe = MockStripe::default();
    let r = execute(&pool, &stripe, &run_id, Utc::now()).await;
    assert_eq!(r.items[0].outcome, "not_payable");
    assert!(r.items[0].reason.as_deref().unwrap().contains("1 of 2"));
    assert_eq!(transfers(&stripe), 0);
    assert_eq!(status(&pool, &e.id).await, PENDING);

    let (carol, _) = member(&pool, "carol", &[scopes::PAYOUTS_APPROVE], &[]).await;
    runs::approve_run(&pool, &dir(), &carol, &run_id).await.unwrap();
    let r = execute(&pool, &stripe, &run_id, Utc::now()).await;
    assert_eq!(r.items[0].outcome, "paid");
    assert_eq!(transfers(&stripe), 1);
}

#[tokio::test]
async fn approver_below_min_rank_does_not_count() {
    let pool = pool().await;
    add_contributor(&pool, "alice", Some("acct_1"), true).await;
    let mut p = base_policy();
    p.approval.min_approver_rank = 60;
    store(&pool, &p).await;
    let e = row(&pool, 1, "alice").await;
    let run_id = prepare(&pool, Utc::now()).await;
    let (bob, _) = member(&pool, "bob", &[scopes::PAYOUTS_APPROVE], &[]).await; // unranked
    runs::approve_run(&pool, &dir(), &bob, &run_id).await.unwrap();

    let stripe = MockStripe::default();
    let r = execute(&pool, &stripe, &run_id, Utc::now()).await;
    assert_eq!(r.items[0].outcome, "not_payable");
    assert_eq!(transfers(&stripe), 0);

    let (owner, _) = member(&pool, "owner1", &[scopes::PAYOUTS_APPROVE], &[]).await; // rank 1000
    runs::approve_run(&pool, &dir(), &owner, &run_id).await.unwrap();
    let r = execute(&pool, &stripe, &run_id, Utc::now()).await;
    assert_eq!(r.items[0].outcome, "paid");
    assert_eq!(status(&pool, &e.id).await, PAID);
}

// ------------------------------------------------------------------ the other payable checks

async fn approved_row(pool: &SqlitePool, pr: i64, user: &str) -> LedgerEntry {
    let e = row(pool, pr, user).await;
    let (bob, _) = member(pool, "bob", &[scopes::PAYOUTS_APPROVE], &[]).await;
    approvals::approve_row(pool, &dir(), &bob, &e.id).await.unwrap();
    e
}

/// Make `id` sort after rows recorded in the same second (runs evaluate oldest first).
async fn make_later(pool: &SqlitePool, id: &str) {
    sqlx::query("UPDATE payout_ledger SET created_at = datetime(created_at, '+1 second') WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn hold_not_passed_pays_nothing() {
    let pool = pool().await;
    add_contributor(&pool, "alice", Some("acct_1"), true).await;
    let mut p = base_policy();
    p.limits.hold_days = Some(7);
    store(&pool, &p).await;
    let e = approved_row(&pool, 1, "alice").await;
    let now = Utc::now();
    let b = plan_blockers(&pool, now).await;
    assert!(matches!(b[0].1.as_slice(), [Blocker::Hold { .. }]), "{:?}", b);

    let run_id = prepare(&pool, now).await;
    let stripe = MockStripe::default();
    assert_eq!(execute(&pool, &stripe, &run_id, now + Duration::days(6)).await.items[0].outcome, "not_payable");
    assert_eq!(transfers(&stripe), 0);
    assert_eq!(execute(&pool, &stripe, &run_id, now + Duration::days(8)).await.items[0].outcome, "paid");
    assert_eq!(status(&pool, &e.id).await, PAID);
}

#[tokio::test]
async fn disabled_policy_pays_nothing() {
    let pool = pool().await;
    add_contributor(&pool, "alice", Some("acct_1"), true).await;
    store(&pool, &base_policy()).await;
    let e = approved_row(&pool, 1, "alice").await;
    let run_id = prepare(&pool, Utc::now()).await;

    let mut off = base_policy();
    off.enabled = false;
    store(&pool, &off).await;
    let b = plan_blockers(&pool, Utc::now()).await;
    assert!(b[0].1.contains(&Blocker::PolicyDisabled));
    // nothing is prepared...
    assert!(matches!(
        runs::create_run(&pool, &cfg(), &dir(), &Principal::legacy_admin(), false, Utc::now()).await,
        Err(PayoutError::PolicyDisabled)
    ));
    // ...or paid
    let stripe = MockStripe::default();
    assert_eq!(execute(&pool, &stripe, &run_id, Utc::now()).await.items[0].outcome, "not_payable");
    assert_eq!(transfers(&stripe), 0);
    assert_eq!(status(&pool, &e.id).await, PENDING);
}

#[tokio::test]
async fn blocked_payee_is_not_paid() {
    let pool = pool().await;
    add_contributor(&pool, "alice", Some("acct_1"), true).await;
    store(&pool, &base_policy()).await;
    let e = approved_row(&pool, 1, "alice").await;
    approvals::block_payee(&pool, &Principal::legacy_admin(), "ALICE", "chargeback review").await.unwrap();
    let b = plan_blockers(&pool, Utc::now()).await;
    assert!(b[0].1.contains(&Blocker::PayeeBlocked), "{:?}", b);

    let run_id = prepare(&pool, Utc::now()).await;
    let stripe = MockStripe::default();
    assert_eq!(execute(&pool, &stripe, &run_id, Utc::now()).await.items[0].outcome, "not_payable");
    assert_eq!(transfers(&stripe), 0);

    assert!(approvals::unblock_payee(&pool, &Principal::legacy_admin(), "alice").await.unwrap());
    assert_eq!(execute(&pool, &stripe, &run_id, Utc::now()).await.items[0].outcome, "paid");
    assert_eq!(status(&pool, &e.id).await, PAID);

    // Contract 13.1: the block and the unblock are both in the audit log; an unblock of a
    // payee that was not blocked records nothing.
    assert!(!approvals::unblock_payee(&pool, &Principal::legacy_admin(), "nobody").await.unwrap());
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT action, target FROM audit_log WHERE action LIKE 'payee.%block%' ORDER BY seq")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        rows,
        vec![
            ("payee.blocked".to_string(), "payee_key:alice".to_string()),
            ("payee.unblocked".to_string(), "payee_key:alice".to_string())
        ]
    );
    assert!(crate::audit::verify(&pool).await.unwrap().ok);
}

// ------------------------------------------------------------------ limits

#[tokio::test]
async fn limit_min_payout() {
    let pool = pool().await;
    add_contributor(&pool, "alice", Some("acct_1"), true).await;
    let mut p = base_policy();
    p.limits.min_payout_cents = Some(3001); // row is 3000
    store(&pool, &p).await;
    approved_row(&pool, 1, "alice").await;
    let b = plan_blockers(&pool, Utc::now()).await;
    assert_eq!(b[0].1, vec![Blocker::BelowMinimum { min: 3001 }]);
    let run_id = prepare(&pool, Utc::now()).await;
    let stripe = MockStripe::default();
    assert_eq!(execute(&pool, &stripe, &run_id, Utc::now()).await.items[0].outcome, "not_payable");
    assert_eq!(transfers(&stripe), 0);
}

#[tokio::test]
async fn limit_max_per_payout() {
    let pool = pool().await;
    add_contributor(&pool, "alice", Some("acct_1"), true).await;
    store(&pool, &base_policy()).await;
    approved_row(&pool, 1, "alice").await; // 3000, recorded before the cap below
    let mut p = base_policy();
    p.limits.max_per_payout_cents = Some(2999);
    store(&pool, &p).await;
    let b = plan_blockers(&pool, Utc::now()).await;
    assert_eq!(b[0].1, vec![Blocker::AboveMaxPerPayout { max: 2999 }]);
    let run_id = prepare(&pool, Utc::now()).await;
    let stripe = MockStripe::default();
    assert_eq!(execute(&pool, &stripe, &run_id, Utc::now()).await.items[0].outcome, "not_payable");
    assert_eq!(transfers(&stripe), 0);
}

#[tokio::test]
async fn limit_max_per_member_per_period() {
    let pool = pool().await;
    add_contributor(&pool, "alice", Some("acct_1"), true).await;
    let mut p = base_policy();
    p.limits.max_per_member_per_period_cents = Some(5000); // two rows of 3000 = 6000
    store(&pool, &p).await;
    let first = approved_row(&pool, 1, "alice").await;
    let second = approved_row(&pool, 2, "alice").await;
    make_later(&pool, &second.id).await;
    let b = plan_blockers(&pool, Utc::now()).await;
    assert!(b.iter().find(|(id, _)| *id == first.id).unwrap().1.is_empty());
    assert_eq!(
        b.iter().find(|(id, _)| *id == second.id).unwrap().1,
        vec![Blocker::MemberPeriodLimit { used: 3000, max: 5000 }]
    );
    let run_id = prepare(&pool, Utc::now()).await;
    let stripe = MockStripe::default();
    let r = execute(&pool, &stripe, &run_id, Utc::now()).await;
    assert_eq!(r.paid_cents, 3000);
    assert_eq!(transfers(&stripe), 1);
    assert_eq!(status(&pool, &second.id).await, PENDING);
}

#[tokio::test]
async fn limit_max_per_run() {
    let pool = pool().await;
    add_contributor(&pool, "alice", Some("acct_1"), true).await;
    add_contributor(&pool, "dave", Some("acct_2"), true).await;
    let mut p = base_policy();
    p.limits.max_per_run_cents = Some(5000);
    store(&pool, &p).await;
    let first = approved_row(&pool, 1, "alice").await;
    let second = approved_row(&pool, 2, "dave").await;
    make_later(&pool, &second.id).await;
    let b = plan_blockers(&pool, Utc::now()).await;
    assert!(b.iter().find(|(id, _)| *id == first.id).unwrap().1.is_empty());
    assert_eq!(
        b.iter().find(|(id, _)| *id == second.id).unwrap().1,
        vec![Blocker::RunLimit { used: 3000, max: 5000 }]
    );
    let run_id = prepare(&pool, Utc::now()).await;
    let stripe = MockStripe::default();
    let r = execute(&pool, &stripe, &run_id, Utc::now()).await;
    assert_eq!(r.paid_cents, 3000);
    assert_eq!(transfers(&stripe), 1);
    // executing again cannot exceed the run cap either
    let r = execute(&pool, &stripe, &run_id, Utc::now()).await;
    assert_eq!(r.paid_cents, 0);
    assert_eq!(transfers(&stripe), 1);
}

/// The limits are also enforced by the atomic claim itself, independent of the evaluator.
#[tokio::test]
async fn claim_guard_enforces_limits_and_blocks_in_sql() {
    let pool = pool().await;
    add_contributor(&pool, "alice", Some("acct_1"), true).await;
    let stripe = MockStripe::default();
    let a = row(&pool, 1, "alice").await; // 3000
    let b = row(&pool, 2, "alice").await; // 3000
    let refused = |r: Result<LedgerEntry, LedgerError>| matches!(r, Err(LedgerError::NotPayable(_)));

    assert!(refused(ledger::pay_guarded(&pool, &stripe, &a.id, "t", &ClaimGuard { min_cents: Some(3001), ..Default::default() }).await));
    assert!(refused(ledger::pay_guarded(&pool, &stripe, &a.id, "t", &ClaimGuard { max_cents: Some(2999), ..Default::default() }).await));
    sqlx::query("INSERT INTO payout_blocks (payee_key, reason, created_by) VALUES ('alice', 'x', 't')").execute(&pool).await.unwrap();
    assert!(refused(ledger::pay_guarded(&pool, &stripe, &a.id, "t", &ClaimGuard { check_blocks: true, ..Default::default() }).await));
    sqlx::query("DELETE FROM payout_blocks").execute(&pool).await.unwrap();
    assert_eq!(transfers(&stripe), 0);

    let window = ("2000-01-01 00:00:00".to_string(), "2100-01-01 00:00:00".to_string(), 5000);
    let g = ClaimGuard { member_period: Some(window), ..Default::default() };
    assert_eq!(ledger::pay_guarded(&pool, &stripe, &a.id, "t", &g).await.unwrap().status, PAID);
    assert!(refused(ledger::pay_guarded(&pool, &stripe, &b.id, "t", &g).await));
    assert_eq!(transfers(&stripe), 1);

    // run cap: a is paid inside run r1; b would push it past 5000
    sqlx::query("INSERT INTO payout_runs (id, trigger_kind, status, created_by) VALUES ('r1', 'manual', 'prepared', 't')").execute(&pool).await.unwrap();
    for id in [&a.id, &b.id] {
        sqlx::query("INSERT INTO payout_run_items (run_id, ledger_id, payee_key, amount_cents, decision) VALUES ('r1', ?, 'alice', 3000, 'include')")
            .bind(id).execute(&pool).await.unwrap();
    }
    assert!(refused(ledger::pay_guarded(&pool, &stripe, &b.id, "t", &ClaimGuard { run_cap: Some(("r1".into(), 5000)), ..Default::default() }).await));
    assert_eq!(transfers(&stripe), 1);
    assert_eq!(status(&pool, &b.id).await, PENDING);
}

// ------------------------------------------------------------------ runs

#[tokio::test]
async fn executing_a_run_twice_transfers_once_per_row() {
    let pool = pool().await;
    add_contributor(&pool, "alice", Some("acct_1"), true).await;
    add_contributor(&pool, "dave", Some("acct_2"), true).await;
    store(&pool, &base_policy()).await;
    let a = approved_row(&pool, 1, "alice").await;
    let d = approved_row(&pool, 2, "dave").await;
    let run_id = prepare(&pool, Utc::now()).await;
    let stripe = MockStripe::default();
    let r1 = execute(&pool, &stripe, &run_id, Utc::now()).await;
    assert_eq!(r1.paid_cents, 6000);
    assert_eq!(r1.run_status, runs::RUN_COMPLETED);
    let r2 = execute(&pool, &stripe, &run_id, Utc::now()).await;
    assert_eq!(r2.paid_cents, 0);
    assert!(r2.items.iter().all(|i| i.outcome == "already_paid"));
    let t = stripe.transfers.lock().unwrap();
    assert_eq!(t.len(), 2, "one transfer per row");
    let keys: Vec<_> = t.iter().map(|x| x.idempotency_key.clone()).collect();
    assert!(keys.contains(&format!("dsync-ledger-{}", a.id)));
    assert!(keys.contains(&format!("dsync-ledger-{}", d.id)));
}

#[tokio::test]
async fn a_dry_run_persists_nothing() {
    let pool = pool().await;
    store(&pool, &base_policy()).await;
    approved_row(&pool, 1, "alice").await;
    let count = |t: &'static str| {
        let pool = pool.clone();
        async move { sqlx::query_scalar::<_, i64>(&format!("SELECT COUNT(*) FROM {t}")).fetch_one(&pool).await.unwrap() }
    };
    let before = (count("payout_runs").await, count("payout_run_items").await, count("payout_approvals").await);
    let r = runs::create_run(&pool, &cfg(), &dir(), &Principal::legacy_admin(), true, Utc::now()).await.unwrap();
    match r {
        CreateRunResult::DryRun(plan) => {
            assert_eq!(plan.items.len(), 1);
            assert_eq!(plan.items[0].decision, "include");
            assert!(plan.items[0].payable_now);
        }
        _ => panic!("dry run must not persist"),
    }
    let after = (count("payout_runs").await, count("payout_run_items").await, count("payout_approvals").await);
    assert_eq!(before, after);
    assert_eq!(after.0, 0);
}

#[tokio::test]
async fn cancelled_runs_cannot_be_approved_or_executed() {
    let pool = pool().await;
    add_contributor(&pool, "alice", Some("acct_1"), true).await;
    approved_row(&pool, 1, "alice").await;
    let run_id = prepare(&pool, Utc::now()).await;
    runs::cancel_run(&pool, &Principal::legacy_admin(), &run_id, Utc::now()).await.unwrap();
    let stripe = MockStripe::default();
    assert!(matches!(
        runs::execute_run(&pool, &stripe, &cfg(), &dir(), &Principal::legacy_admin(), &run_id, Utc::now()).await,
        Err(PayoutError::InvalidState(_))
    ));
    let (bob, _) = member(&pool, "bob", &[scopes::PAYOUTS_APPROVE], &[]).await;
    assert!(matches!(runs::approve_run(&pool, &dir(), &bob, &run_id).await, Err(PayoutError::InvalidState(_))));
    assert_eq!(transfers(&stripe), 0);
}

#[tokio::test]
async fn run_functions_require_their_scopes() {
    let pool = pool().await;
    let e = row(&pool, 1, "alice").await;
    let run_id = prepare(&pool, Utc::now()).await;
    let (reader, _) = member(&pool, "bob", &[scopes::PAYOUTS_READ], &[]).await;
    let stripe = MockStripe::default();
    let missing = |r: Result<(), PayoutError>| matches!(r, Err(PayoutError::Auth(AuthError::MissingScope(_))));
    assert!(missing(runs::create_run(&pool, &cfg(), &dir(), &reader, true, Utc::now()).await.map(|_| ())));
    assert!(missing(runs::approve_run(&pool, &dir(), &reader, &run_id).await.map(|_| ())));
    assert!(missing(runs::execute_run(&pool, &stripe, &cfg(), &dir(), &reader, &run_id, Utc::now()).await.map(|_| ())));
    assert!(missing(runs::cancel_run(&pool, &reader, &run_id, Utc::now()).await.map(|_| ())));
    assert!(missing(approvals::approve_row(&pool, &dir(), &reader, &e.id).await.map(|_| ())));
    assert!(missing(approvals::block_payee(&pool, &reader, "alice", "x").await));
    assert!(runs::get_run(&pool, &reader, &run_id).await.is_ok());
    assert_eq!(transfers(&stripe), 0);
}

// ------------------------------------------------------------------ schedule

fn weekly_friday_17() -> Schedule {
    Schedule { cadence: Cadence::Weekly, day: Some(ScheduleDay::Weekday("friday".into())), hour_utc: Some(17), mode: ScheduleMode::Prepare }
}

fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
}

#[test]
fn schedule_period_keys_are_pure_and_stable() {
    let s = weekly_friday_17();
    // 2026-11-13 is a Friday.
    assert_eq!(current_slot(&s, at(2026, 11, 13, 16, 59)).unwrap().key, "weekly-2026-11-06");
    assert_eq!(current_slot(&s, at(2026, 11, 13, 17, 0)).unwrap().key, "weekly-2026-11-13");
    assert_eq!(current_slot(&s, at(2026, 11, 19, 23, 0)).unwrap().key, "weekly-2026-11-13");
    assert_eq!(current_slot(&s, at(2026, 11, 20, 17, 0)).unwrap().key, "weekly-2026-11-20");
    let slot = current_slot(&s, at(2026, 11, 15, 0, 0)).unwrap();
    assert_eq!((slot.start, slot.end), (at(2026, 11, 13, 17, 0), at(2026, 11, 20, 17, 0)));

    let bi = Schedule { cadence: Cadence::Biweekly, ..weekly_friday_17() };
    let k1 = current_slot(&bi, at(2026, 11, 13, 18, 0)).unwrap();
    let k2 = current_slot(&bi, at(2026, 11, 20, 18, 0)).unwrap();
    let k3 = current_slot(&bi, at(2026, 11, 27, 18, 0)).unwrap();
    assert_eq!(k1.end - k1.start, Duration::days(14));
    assert!(k1.key == k2.key || k2.key == k3.key, "a biweekly key spans two weeks");
    assert_ne!(k1.key, k3.key);

    let mo = Schedule { cadence: Cadence::Monthly, day: Some(ScheduleDay::MonthDay(1)), hour_utc: Some(9), mode: ScheduleMode::Auto };
    assert_eq!(current_slot(&mo, at(2026, 11, 1, 8, 0)).unwrap().key, "monthly-2026-10");
    assert_eq!(current_slot(&mo, at(2026, 11, 1, 9, 0)).unwrap().key, "monthly-2026-11");
    assert_eq!(current_slot(&mo, at(2027, 1, 1, 0, 0)).unwrap().key, "monthly-2026-12");

    let manual = Schedule { cadence: Cadence::Manual, day: None, hour_utc: None, mode: ScheduleMode::Prepare };
    assert!(current_slot(&manual, at(2026, 11, 13, 17, 0)).is_none());
}

#[test]
fn run_is_due_once_per_period_and_never_when_disabled_or_manual() {
    let mut p = base_policy();
    p.schedule = weekly_friday_17();
    let now = at(2026, 11, 13, 17, 5);
    assert_eq!(run_due(&p, now, None).as_deref(), Some("weekly-2026-11-13"));
    assert_eq!(run_due(&p, now, Some("weekly-2026-11-06")).as_deref(), Some("weekly-2026-11-13"));
    assert_eq!(run_due(&p, now, Some("weekly-2026-11-13")), None);
    p.enabled = false;
    assert_eq!(run_due(&p, now, None), None);
    p.enabled = true;
    p.schedule.cadence = Cadence::Manual;
    assert_eq!(run_due(&p, now, None), None);
}

fn ctx_with(pool: &SqlitePool, stripe: Arc<MockStripe>) -> ApiContext {
    ApiContext { pool: pool.clone(), payments: Arc::new(cfg()), stripe, directory: Arc::new(dir()) }
}

#[tokio::test]
async fn one_scheduled_run_per_period() {
    let pool = pool().await;
    add_contributor(&pool, "alice", Some("acct_1"), true).await;
    let mut p = base_policy();
    p.schedule = Schedule { mode: ScheduleMode::Auto, ..weekly_friday_17() };
    store(&pool, &p).await;
    let e = approved_row(&pool, 1, "alice").await;
    let stripe = Arc::new(MockStripe::default());
    let ctx = ctx_with(&pool, stripe.clone());

    let t0 = at(2026, 11, 13, 17, 1);
    let first = schedule::scheduler_tick(&ctx, t0).await.unwrap();
    assert!(matches!(&first, TickOutcome::Created { period_key, executed: true, .. } if period_key == "weekly-2026-11-13"));
    assert_eq!(schedule::scheduler_tick(&ctx, t0 + Duration::minutes(1)).await.unwrap(), TickOutcome::Idle);
    assert_eq!(schedule::scheduler_tick(&ctx, t0 + Duration::days(3)).await.unwrap(), TickOutcome::Idle);
    // auto mode paid the already-approved row, exactly once
    assert_eq!(status(&pool, &e.id).await, PAID);
    assert_eq!(transfers(&stripe), 1);

    // even bypassing run_due, the period key is unique per org
    let system = Principal::system("scheduler", &[scopes::PAYOUTS_PREPARE]);
    assert!(runs::create_scheduled_run(&pool, &cfg(), &dir(), &system, "weekly-2026-11-13", t0).await.unwrap().is_none());

    let next = schedule::scheduler_tick(&ctx, at(2026, 11, 20, 17, 0)).await.unwrap();
    assert!(matches!(&next, TickOutcome::Created { period_key, .. } if period_key == "weekly-2026-11-20"));
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payout_runs WHERE period_key IS NOT NULL").fetch_one(&pool).await.unwrap();
    assert_eq!(n, 2);
    assert_eq!(transfers(&stripe), 1);
}

#[tokio::test]
async fn prepare_mode_schedule_pays_nothing_by_itself() {
    let pool = pool().await;
    add_contributor(&pool, "alice", Some("acct_1"), true).await;
    let mut p = base_policy();
    p.schedule = weekly_friday_17();
    store(&pool, &p).await;
    approved_row(&pool, 1, "alice").await;
    let stripe = Arc::new(MockStripe::default());
    let out = schedule::scheduler_tick(&ctx_with(&pool, stripe.clone()), at(2026, 11, 13, 17, 1)).await.unwrap();
    assert!(matches!(out, TickOutcome::Created { executed: false, .. }));
    assert_eq!(transfers(&stripe), 0);
}

// ------------------------------------------------------------------ legacy route

async fn legacy(pool: &SqlitePool, stripe: &MockStripe, id: &str) -> Result<LegacyApproveOutcome, PayoutError> {
    approvals::legacy_admin_approve(pool, stripe, &cfg(), &dir(), id, "body-says-mallory", Utc::now()).await
}

#[tokio::test]
async fn legacy_route_obeys_the_policy() {
    let pool = pool().await;
    add_contributor(&pool, "alice", Some("acct_1"), true).await;
    let stripe = MockStripe::default();

    // No stored policy: today's behaviour (pays at once; decided_by from the body).
    let a = row(&pool, 1, "alice").await;
    match legacy(&pool, &stripe, &a.id).await.unwrap() {
        LegacyApproveOutcome::Transferred { entry } => {
            assert_eq!(entry.status, PAID);
            assert_eq!(entry.decided_by.as_deref(), Some("body-says-mallory"));
        }
        other => panic!("expected a transfer, got {other:?}"),
    }
    assert_eq!(transfers(&stripe), 1);

    // Two approvers required: the admin token is one approval and nothing is paid.
    let mut two = base_policy();
    two.approval.approvals_required = 2;
    store(&pool, &two).await;
    let b = row(&pool, 2, "alice").await;
    assert!(matches!(legacy(&pool, &stripe, &b.id).await.unwrap(), LegacyApproveOutcome::ApprovalRecorded { .. }));
    assert_eq!(transfers(&stripe), 1);
    let approver: String = sqlx::query_scalar("SELECT approver_member_id FROM payout_approvals WHERE ledger_id = ?")
        .bind(&b.id).fetch_one(&pool).await.unwrap();
    assert_eq!(approver, "admin-token", "the body cannot name the approver");

    // Single approver but a hold: still nothing.
    let mut hold = base_policy();
    hold.limits.hold_days = Some(3);
    store(&pool, &hold).await;
    assert!(matches!(legacy(&pool, &stripe, &b.id).await.unwrap(), LegacyApproveOutcome::ApprovalRecorded { .. }));
    // Single approver with a rank floor: admin-token has no rank, so nothing.
    let mut ranked = base_policy();
    ranked.approval.min_approver_rank = 1;
    store(&pool, &ranked).await;
    assert!(matches!(legacy(&pool, &stripe, &b.id).await.unwrap(), LegacyApproveOutcome::ApprovalRecorded { .. }));
    // Disabled: nothing.
    let mut off = base_policy();
    off.enabled = false;
    store(&pool, &off).await;
    assert!(matches!(legacy(&pool, &stripe, &b.id).await.unwrap(), LegacyApproveOutcome::ApprovalRecorded { .. }));
    assert_eq!(transfers(&stripe), 1);
    assert_eq!(status(&pool, &b.id).await, PENDING);

    // Single approver, every check passes: paid, by admin-token.
    store(&pool, &base_policy()).await;
    match legacy(&pool, &stripe, &b.id).await.unwrap() {
        LegacyApproveOutcome::Transferred { entry } => {
            assert_eq!(entry.status, PAID);
            assert_eq!(entry.decided_by.as_deref(), Some("admin-token"));
        }
        other => panic!("expected a transfer, got {other:?}"),
    }
    assert_eq!(transfers(&stripe), 2);
}

// ------------------------------------------------------------------ HTTP

macro_rules! app {
    ($ctx:expr) => {
        atest::init_service(App::new().app_data(web::Data::new($ctx)).configure(api::configure)).await
    };
}

#[actix_web::test]
async fn the_request_body_cannot_name_the_approver_or_actor() {
    let pool = pool().await;
    let e = row(&pool, 1, "alice").await;
    let (_, bob_token) = member(&pool, "bob", ALL_PAYOUT, &[]).await;
    let app = app!(ctx_with(&pool, Arc::new(MockStripe::default())));
    let auth = ("Authorization", format!("Bearer {bob_token}"));

    // An actor field in a run body is rejected outright.
    let req = atest::TestRequest::post()
        .uri("/api/v1/payouts/runs")
        .insert_header(auth.clone())
        .set_json(json!({"dry_run": false, "created_by": "mallory"}))
        .to_request();
    assert_eq!(atest::call_service(&app, req).await.status(), 400);

    let req = atest::TestRequest::post()
        .uri("/api/v1/payouts/runs")
        .insert_header(auth.clone())
        .set_json(json!({"dry_run": false}))
        .to_request();
    let resp = atest::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = atest::read_body_json(resp).await;
    let run_id = body["run"]["run"]["id"].as_str().unwrap().to_string();
    assert_eq!(body["run"]["run"]["created_by"], "bob");

    // Approve with a body naming someone else: the approver is still the token's member.
    for uri in [format!("/api/v1/payouts/runs/{run_id}/approve"), format!("/api/v1/payouts/ledger/{}/approve", e.id)] {
        let req = atest::TestRequest::post()
            .uri(&uri)
            .insert_header(auth.clone())
            .set_json(json!({"approver_member_id": "mallory", "decided_by": "mallory", "member_id": "mallory"}))
            .to_request();
        assert_eq!(atest::call_service(&app, req).await.status(), 200, "{uri}");
    }
    let approvers: Vec<String> = sqlx::query_scalar("SELECT approver_member_id FROM payout_approvals").fetch_all(&pool).await.unwrap();
    assert_eq!(approvers, vec!["bob".to_string()]);
}

#[actix_web::test]
async fn every_auth_failure_is_the_same_401_and_missing_scope_is_403() {
    let pool = pool().await;
    let (_, read_token) = member(&pool, "bob", &[scopes::PAYOUTS_READ], &[]).await;
    let (_, revoked_token) = member(&pool, "carol", &[scopes::PAYOUTS_READ], &[]).await;
    sqlx::query("UPDATE api_tokens SET revoked_at = datetime('now') WHERE member_id = 'carol'").execute(&pool).await.unwrap();
    let (id, _) = crate::auth::tokens::parse_token(&read_token).unwrap();
    let wrong_secret = format!("msync_{id}_{}", "f".repeat(64));
    let app = app!(ctx_with(&pool, Arc::new(MockStripe::default())));

    let mut bodies = Vec::new();
    for header in [None, Some("Bearer nonsense".to_string()), Some(format!("Bearer {wrong_secret}")), Some(format!("Bearer {revoked_token}"))] {
        let mut req = atest::TestRequest::get().uri("/api/v1/payouts/ledger");
        if let Some(h) = header {
            req = req.insert_header(("Authorization", h));
        }
        let resp = atest::call_service(&app, req.to_request()).await;
        assert_eq!(resp.status(), 401);
        bodies.push(atest::read_body(resp).await);
    }
    assert!(bodies.windows(2).all(|w| w[0] == w[1]), "identical 401 bodies");

    let ok = atest::TestRequest::get().uri("/api/v1/payouts/ledger").insert_header(("Authorization", format!("Bearer {read_token}"))).to_request();
    assert_eq!(atest::call_service(&app, ok).await.status(), 200);
    let forbidden = atest::TestRequest::post()
        .uri("/api/v1/payouts/runs")
        .insert_header(("Authorization", format!("Bearer {read_token}")))
        .set_json(json!({"dry_run": true}))
        .to_request();
    assert_eq!(atest::call_service(&app, forbidden).await.status(), 403);
}

#[actix_web::test]
async fn tokens_endpoint_accepts_bootstrap_admin_token_only_as_bootstrap() {
    let pool = pool().await;
    let mut c = cfg();
    c.admin_token = Some("legacy-admin-secret".into());
    let ctx = ApiContext { payments: Arc::new(c), ..ctx_with(&pool, Arc::new(MockStripe::default())) };
    let app = app!(ctx);
    let body = json!({"label": "ci", "member_id": "bob", "scopes": ["payouts.read"], "payee_keys": ["bob"]});

    let bad = atest::TestRequest::post().uri("/api/v1/tokens").insert_header(("Authorization", "Bearer wrong")).set_json(&body).to_request();
    assert_eq!(atest::call_service(&app, bad).await.status(), 401);

    let req = atest::TestRequest::post()
        .uri("/api/v1/tokens")
        .insert_header(("Authorization", "Bearer legacy-admin-secret"))
        .set_json(&body)
        .to_request();
    let resp = atest::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);
    let v: serde_json::Value = atest::read_body_json(resp).await;
    let token = v["token"]["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("msync_"));

    // The legacy admin token is NOT accepted on the payouts API.
    let req = atest::TestRequest::get().uri("/api/v1/payouts/ledger").insert_header(("Authorization", "Bearer legacy-admin-secret")).to_request();
    assert_eq!(atest::call_service(&app, req).await.status(), 401);
    // The new token is.
    let req = atest::TestRequest::get().uri("/api/v1/payouts/ledger").insert_header(("Authorization", format!("Bearer {token}"))).to_request();
    assert_eq!(atest::call_service(&app, req).await.status(), 200);
}

#[test]
fn evaluate_is_pure_over_facts() {
    // Sanity check of the full rule on hand-built facts (the DB-backed tests above prove reachability).
    let p = base_policy();
    let entry = LedgerEntry {
        id: "x".into(),
        pr_id: 1,
        pr_number: 1,
        merge_sha: "s".into(),
        repo: None,
        title: "t".into(),
        github_username: "alice".into(),
        contributor_id: None,
        branch: "main".into(),
        base_amount_cents: 2000,
        merit_points: 50,
        amount_cents: 3000,
        currency: "usd".into(),
        status: PENDING.into(),
        stripe_transfer_id: None,
        failure_reason: None,
        decided_by: None,
        created_at: "2026-11-01 00:00:00".into(),
        updated_at: "2026-11-01 00:00:00".into(),
        source_kind: "merit".into(),
        source_ref: None,
        payee_id: None,
        pay_after: None,
    };
    let facts = RowFacts { entry, approvers: vec![], blocked: false, verified_identity: false, payee_rank: None, payee_roles: vec![], payee_inactive: None };
    assert_eq!(evaluate(&p, &facts, Utc::now(), &Usage::default()), vec![Blocker::Approvals { have: 0, need: 1 }]);
}

// ------------------------------------------------------------------ verified identity

/// A login typed into chat `link` fills `contributors.discord_user_id` and an unverified chat
/// binding. Neither proves anything, so neither makes the login eligible under
/// `require_verified_identity`. A proven chat binding or a CRA listing does.
#[tokio::test]
async fn a_typed_in_link_no_longer_makes_a_login_eligible() {
    let pool = pool().await;
    let mut p = base_policy();
    p.eligibility.require_verified_identity = true;
    store(&pool, &p).await;

    // alice: what a typed-in `/link alice` leaves behind (discord_user_id = 42, unverified binding).
    add_contributor(&pool, "alice", Some("acct_alice"), true).await;
    sqlx::query(
        "INSERT INTO chat_identities (platform, team_or_guild, user_id, contributor_id, verified, source)
         SELECT 'discord', '', '42', id, 0, 'link' FROM contributors WHERE github_username = 'alice'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let e = row(&pool, 1, "alice").await;
    let unverified = Blocker::Ineligible { reason: "payee identity not verified".into() };

    let facts = approvals::load_facts(&pool, &dir(), e.clone()).await.unwrap();
    assert!(!facts.verified_identity, "a typed-in link must not verify a login");
    assert!(evaluate(&p, &facts, Utc::now(), &Usage::default()).contains(&unverified));
    assert!(plan_blockers(&pool, Utc::now()).await.iter().any(|(id, b)| id == &e.id && b.contains(&unverified)));

    // A proven binding (a future OAuth link sets verified = 1) does.
    sqlx::query("UPDATE chat_identities SET verified = 1").execute(&pool).await.unwrap();
    let facts = approvals::load_facts(&pool, &dir(), e.clone()).await.unwrap();
    assert!(facts.verified_identity);
    assert!(!evaluate(&p, &facts, Utc::now(), &Usage::default()).contains(&unverified));
}

#[tokio::test]
async fn a_cra_listing_verifies_a_login() {
    let pool = pool().await;
    assert!(!approvals::payee_identity_verified(&pool, "ada-example").await.unwrap(), "no CRA: nothing attested");
    let files = crate::cra::CraFiles {
        org: include_str!("../../examples/cra/eustress-engine/.msync/org.toml").into(),
        roles: include_str!("../../examples/cra/eustress-engine/.msync/roles.toml").into(),
        members: include_str!("../../examples/cra/eustress-engine/.msync/members.toml").into(),
        merit: include_str!("../../examples/cra/eustress-engine/.msync/merit.toml").into(),
        payouts: include_str!("../../examples/cra/eustress-engine/.msync/payouts.toml").into(),
    };
    let repo = crate::cra::CraRepo::new(crate::foundry::FoundryProvider::GitHub, "github.com", "example-eustress/cra");
    let sha = "a".repeat(40);
    let a = crate::cra::store::Attempt {
        org_id: crate::cra::loader::ORG_ID,
        org_slug: Some("eustress-engine"),
        repo: &repo,
        commit_sha: &sha,
        files: Some(&files),
        ctx: None,
        violations: &[],
        plan_markdown: None,
        retryable: false,
    };
    assert!(matches!(crate::cra::store::activate(&pool, &a, None).await.unwrap(), crate::cra::store::Activation::Activated(_)));
    assert!(approvals::payee_identity_verified(&pool, "ada-example").await.unwrap());
    assert!(approvals::payee_identity_verified(&pool, "Ada-Example").await.unwrap(), "logins are case-insensitive");
    assert!(approvals::payee_identity_verified(&pool, "github:github.com:ada-example").await.unwrap());
    assert!(!approvals::payee_identity_verified(&pool, "gitlab:ada-example").await.unwrap(), "another provider is another identity");
    assert!(!approvals::payee_identity_verified(&pool, "mallory").await.unwrap());
    // The activation was audited in its own transaction.
    let entries = crate::audit::list(&pool, &crate::audit::AuditFilter { action: Some("cra.snapshot.activated".into()), ..Default::default() }).await.unwrap();
    assert_eq!(entries.entries.len(), 1);
    assert_eq!(entries.entries[0].detail["commit_sha"], serde_json::json!(sha));
}
