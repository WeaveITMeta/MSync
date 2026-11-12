use super::config::PaymentConfig;
use super::ledger::{self, LedgerError, MergeEvent, FAILED, PAID, PENDING, REJECTED};
use super::routes::account_transfers_enabled;
use super::signature::{stripe_v1_signature, token_matches, verify_stripe_signature};
use super::stripe::{StripeApi, TransferRequest};
use anyhow::{bail, Result};
use async_trait::async_trait;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::sync::Mutex;

#[derive(Default)]
pub(super) struct MockStripe {
    pub(super) transfers: Mutex<Vec<TransferRequest>>,
    pub(super) accounts_created: Mutex<u32>,
    pub(super) fail: bool,
}

#[async_trait]
impl StripeApi for MockStripe {
    async fn create_express_account(&self, _u: &str) -> Result<String> {
        *self.accounts_created.lock().unwrap() += 1;
        Ok("acct_mock".into())
    }
    async fn create_account_link(&self, a: &str, _r: &str, _ret: &str) -> Result<String> {
        Ok(format!("https://connect.stripe.test/{a}"))
    }
    async fn create_transfer(&self, req: &TransferRequest) -> Result<String> {
        self.transfers.lock().unwrap().push(req.clone());
        if self.fail {
            bail!("card_declined (mock)");
        }
        Ok(format!("tr_mock_{}", self.transfers.lock().unwrap().len()))
    }
}

pub(super) fn cfg() -> PaymentConfig {
    PaymentConfig::from_lookup(|k| match k {
        "DSYNC_PAY_BRANCHES" => Some("main, upcoming".into()),
        "DSYNC_PAY_BASE_CENTS_MAIN" => Some("2000".into()),
        "DSYNC_PAY_BASE_CENTS_UPCOMING" => Some("1000".into()),
        "DSYNC_PAY_MERIT_MULTIPLIER" => Some("0.01".into()),
        "DSYNC_PAY_MAX_CENTS" => Some("5000".into()),
        _ => None,
    })
}

pub(super) async fn pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    pool
}

pub(super) async fn add_contributor(pool: &SqlitePool, gh: &str, acct: Option<&str>, enabled: bool) {
    sqlx::query(
        "INSERT INTO contributors (id, github_username, discord_user_id, created_at, updated_at, stripe_account_id, stripe_payouts_enabled)
         VALUES (?, ?, 42, datetime('now'), datetime('now'), ?, ?)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(gh)
    .bind(acct)
    .bind(enabled as i64)
    .execute(pool)
    .await
    .unwrap();
}

pub(super) fn merge(pr_id: i64, sha: &str, branch: &str) -> MergeEvent {
    MergeEvent {
        pr_id,
        pr_number: pr_id,
        merge_sha: sha.into(),
        repo: Some("org/eustress".into()),
        title: "Fix thing".into(),
        github_username: "alice".into(),
        branch: branch.into(),
        merit_points: 50,
    }
}

#[test]
fn amount_uses_branch_base_multiplier_and_cap() {
    let c = cfg();
    assert_eq!(c.amount_for("main", 50), Some((2000, 3000))); // 2000 * 1.5
    assert_eq!(c.amount_for("upcoming", 0), Some((1000, 1000)));
    assert_eq!(c.amount_for("main", 1000), Some((2000, 5000))); // capped
    assert_eq!(c.amount_for("feature/x", 50), None);
}

#[tokio::test]
async fn ledger_is_idempotent_on_pr_id_and_merge_sha() {
    let pool = pool().await;
    let c = cfg();
    let first = ledger::record_merge(&pool, &c, &merge(1, "abc", "main")).await.unwrap();
    assert!(first.is_some());
    let e = first.unwrap();
    assert_eq!(e.status, PENDING);
    assert_eq!(e.amount_cents, 3000);

    // Duplicate webhook delivery: no new row
    assert!(ledger::record_merge(&pool, &c, &merge(1, "abc", "main")).await.unwrap().is_none());
    // Untracked branch: no row
    assert!(ledger::record_merge(&pool, &c, &merge(2, "def", "dev")).await.unwrap().is_none());
    // Different merge sha for same PR (e.g. reopened + re-merged): new row
    assert!(ledger::record_merge(&pool, &c, &merge(1, "xyz", "main")).await.unwrap().is_some());

    assert_eq!(ledger::list(&pool, None).await.unwrap().len(), 2);
}

#[tokio::test]
async fn approve_requires_onboarded_account_and_never_transfers_otherwise() {
    let pool = pool().await;
    let stripe = MockStripe::default();
    let e = ledger::record_merge(&pool, &cfg(), &merge(1, "abc", "main")).await.unwrap().unwrap();

    // No contributor / account at all
    assert!(matches!(ledger::approve(&pool, &stripe, &e.id, "admin").await, Err(LedgerError::NotPayable(_))));
    // Account exists but onboarding incomplete
    add_contributor(&pool, "alice", Some("acct_1"), false).await;
    assert!(matches!(ledger::approve(&pool, &stripe, &e.id, "admin").await, Err(LedgerError::NotPayable(_))));

    assert!(stripe.transfers.lock().unwrap().is_empty());
    assert_eq!(ledger::get(&pool, &e.id).await.unwrap().unwrap().status, PENDING);
}

#[tokio::test]
async fn approve_pays_once_with_idempotency_key() {
    let pool = pool().await;
    add_contributor(&pool, "alice", Some("acct_1"), true).await;
    let stripe = MockStripe::default();
    let e = ledger::record_merge(&pool, &cfg(), &merge(1, "abc", "main")).await.unwrap().unwrap();

    // Recording a merge never transfers
    assert!(stripe.transfers.lock().unwrap().is_empty());

    let paid = ledger::approve(&pool, &stripe, &e.id, "maintainer").await.unwrap();
    assert_eq!(paid.status, PAID);
    assert_eq!(paid.stripe_transfer_id.as_deref(), Some("tr_mock_1"));
    assert_eq!(paid.decided_by.as_deref(), Some("maintainer"));
    {
        let t = stripe.transfers.lock().unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].amount_cents, 3000);
        assert_eq!(t[0].destination, "acct_1");
        assert_eq!(t[0].idempotency_key, format!("dsync-ledger-{}", e.id));
    }

    // Second approve is refused and does not transfer again
    assert!(matches!(ledger::approve(&pool, &stripe, &e.id, "maintainer").await, Err(LedgerError::InvalidState(..))));
    assert_eq!(stripe.transfers.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn failed_transfer_marks_failed_and_can_retry_with_same_key() {
    let pool = pool().await;
    add_contributor(&pool, "alice", Some("acct_1"), true).await;
    let failing = MockStripe { fail: true, ..Default::default() };
    let e = ledger::record_merge(&pool, &cfg(), &merge(1, "abc", "main")).await.unwrap().unwrap();

    let failed = ledger::approve(&pool, &failing, &e.id, "admin").await.unwrap();
    assert_eq!(failed.status, FAILED);
    assert!(failed.failure_reason.unwrap().contains("card_declined"));

    let ok = MockStripe::default();
    let paid = ledger::approve(&pool, &ok, &e.id, "admin").await.unwrap();
    assert_eq!(paid.status, PAID);
    assert_eq!(
        failing.transfers.lock().unwrap()[0].idempotency_key,
        ok.transfers.lock().unwrap()[0].idempotency_key
    );
}

#[tokio::test]
async fn rejected_entries_cannot_be_approved() {
    let pool = pool().await;
    add_contributor(&pool, "alice", Some("acct_1"), true).await;
    let stripe = MockStripe::default();
    let e = ledger::record_merge(&pool, &cfg(), &merge(1, "abc", "main")).await.unwrap().unwrap();

    let r = ledger::reject(&pool, &e.id, "admin", "duplicate work").await.unwrap();
    assert_eq!(r.status, REJECTED);
    assert!(matches!(ledger::approve(&pool, &stripe, &e.id, "admin").await, Err(LedgerError::InvalidState(..))));
    assert!(stripe.transfers.lock().unwrap().is_empty());
    assert!(matches!(ledger::approve(&pool, &stripe, "nope", "admin").await, Err(LedgerError::NotFound)));
}

#[tokio::test]
async fn account_updated_toggles_payout_eligibility() {
    let pool = pool().await;
    add_contributor(&pool, "alice", Some("acct_1"), false).await;
    assert_eq!(ledger::update_account_status(&pool, "acct_1", true).await.unwrap(), 1);
    assert_eq!(ledger::update_account_status(&pool, "acct_unknown", true).await.unwrap(), 0);

    let ok = serde_json::json!({"payouts_enabled": true, "capabilities": {"transfers": "active"}});
    let pending = serde_json::json!({"payouts_enabled": true, "capabilities": {"transfers": "pending"}});
    assert!(account_transfers_enabled(&ok));
    assert!(!account_transfers_enabled(&pending));
}

#[test]
fn stripe_signature_verification() {
    let secret = "whsec_test_secret";
    let body = br#"{"type":"account.updated"}"#;
    let t = 1_700_000_000;
    let sig = stripe_v1_signature(secret, t, body);
    let header = format!("t={t},v1={sig}");

    assert!(verify_stripe_signature(secret, body, Some(&header), t + 10, 300));
    // multiple v1 entries (secret rotation), one valid
    assert!(verify_stripe_signature(secret, body, Some(&format!("t={t},v1=00ff,v1={sig}")), t, 300));
    assert!(!verify_stripe_signature(secret, b"tampered", Some(&header), t, 300));
    assert!(!verify_stripe_signature("whsec_other", body, Some(&header), t, 300));
    assert!(!verify_stripe_signature(secret, body, Some(&header), t + 301, 300)); // too old
    assert!(!verify_stripe_signature(secret, body, None, t, 300));
    assert!(!verify_stripe_signature("", body, Some(&header), t, 300));
    assert!(!verify_stripe_signature(secret, body, Some("v1=abcd"), t, 300));
}

#[test]
fn admin_token_comparison() {
    assert!(token_matches("s3cret-token", "s3cret-token"));
    assert!(!token_matches("s3cret-token", "s3cret-tokem"));
    assert!(!token_matches("s3cret-token", ""));
    assert!(!token_matches("", ""));
}
#[tokio::test]
async fn onboarding_creates_account_once_and_requires_linked_discord() {
    use super::onboarding::{self, OnboardError};
    let pool = pool().await;
    let stripe = MockStripe::default();
    let mut c = cfg();
    c.admin_token = Some("admin-secret".into());
    c.public_url = "https://dsync.example".into();

    assert!(matches!(
        onboarding::onboarding_link_for_github(&pool, &stripe, &c, "ghost").await,
        Err(OnboardError::UnknownContributor)
    ));
    sqlx::query("INSERT INTO contributors (id, github_username, created_at, updated_at) VALUES ('x', 'unlinked', datetime('now'), datetime('now'))")
        .execute(&pool).await.unwrap();
    assert!(matches!(
        onboarding::onboarding_link_for_github(&pool, &stripe, &c, "unlinked").await,
        Err(OnboardError::NotLinked)
    ));

    add_contributor(&pool, "alice", None, false).await;
    let first = onboarding::onboarding_link_for_github(&pool, &stripe, &c, "alice").await.unwrap();
    let second = onboarding::onboarding_link_for_github(&pool, &stripe, &c, "alice").await.unwrap();
    assert_eq!(first.account_id, "acct_mock");
    assert_eq!(second.account_id, "acct_mock");
    assert_eq!(*stripe.accounts_created.lock().unwrap(), 1, "account reused, not recreated");
    assert_eq!(onboarding::contributor_for_discord(&pool, 42).await.unwrap(), Some(("alice".into(), false)));
}

#[test]
fn refresh_links_are_signed() {
    use super::onboarding::{refresh_url, verify_refresh_sig, refresh_sig};
    let mut c = cfg();
    c.public_url = "https://dsync.example".into();
    assert_eq!(refresh_url(&c, "acct_1"), "https://dsync.example/payouts/refresh"); // no key -> unsigned
    assert!(!verify_refresh_sig(&c, "acct_1", "anything"));

    c.admin_token = Some("admin-secret".into());
    let sig = refresh_sig(&c, "acct_1").unwrap();
    assert!(refresh_url(&c, "acct_1").contains(&format!("account=acct_1&sig={sig}")));
    assert!(verify_refresh_sig(&c, "acct_1", &sig));
    assert!(!verify_refresh_sig(&c, "acct_2", &sig));
    assert!(!verify_refresh_sig(&c, "acct_1", "deadbeef"));
}
