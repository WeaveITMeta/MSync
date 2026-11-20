//! Audit log: append-only in the database, a chain that names its first broken entry, no fork
//! under concurrent writers, no secrets stored, and strict actions that fail without their entry.

use std::str::FromStr;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

use super::*;
use crate::auth::{create_token, scopes, EnvDirectory, NewToken, TokenCreator};
use crate::payments::ledger::{self, MergeEvent, PAID, PENDING};
use crate::payments::{PaymentConfig, StripeApi, TransferRequest};

async fn pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    pool
}

fn entry(n: usize) -> AuditEntry {
    AuditEntry::new(ActorKind::System, "system:test", "test.event", format!("thing:{n}"), json!({"n": n, "b": "x", "a": [1, 2]}))
}

async fn count(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM audit_log").fetch_one(pool).await.unwrap()
}

/// Every column of every stored entry, joined: what a reader of the table could see.
async fn dump(pool: &SqlitePool) -> String {
    let rows: Vec<(i64, String, String, String, String, String, String, String, String, String)> = sqlx::query_as(
        "SELECT seq, at, org_id, actor_kind, actor, action, target, detail, prev_hash, hash FROM audit_log ORDER BY seq",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    rows.into_iter().map(|r| format!("{r:?}")).collect::<Vec<_>>().join("\n")
}

#[derive(Default)]
struct MockStripe {
    transfers: Mutex<Vec<TransferRequest>>,
    fail_with: Option<String>,
}

#[async_trait]
impl StripeApi for MockStripe {
    async fn create_express_account(&self, login: &str) -> anyhow::Result<String> {
        Ok(format!("acct_{login}"))
    }
    async fn create_account_link(&self, a: &str, _r: &str, _ret: &str) -> anyhow::Result<String> {
        Ok(format!("https://connect.stripe.test/{a}"))
    }
    async fn create_transfer(&self, req: &TransferRequest) -> anyhow::Result<String> {
        self.transfers.lock().unwrap().push(req.clone());
        match &self.fail_with {
            Some(m) => anyhow::bail!("{m}"),
            None => Ok(format!("tr_mock_{}", self.transfers.lock().unwrap().len())),
        }
    }
}

fn pay_cfg() -> PaymentConfig {
    PaymentConfig::from_lookup(|k| match k {
        "MSYNC_PAY_BRANCHES" => Some("main".into()),
        "MSYNC_PAY_BASE_CENTS_MAIN" => Some("2000".into()),
        "MSYNC_PAY_MERIT_MULTIPLIER" => Some("0.01".into()),
        "MSYNC_PAY_MAX_CENTS" => Some("5000".into()),
        _ => None,
    })
}

const ACCT: &str = "acct_1QwErTyUiOpAsDfG";

async fn payable_row(pool: &SqlitePool, pr: i64) -> String {
    sqlx::query(
        "INSERT OR IGNORE INTO contributors (id, github_username, created_at, updated_at, stripe_account_id, stripe_payouts_enabled)
         VALUES (?, 'alice', datetime('now'), datetime('now'), ?, 1)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(ACCT)
    .execute(pool)
    .await
    .unwrap();
    let ev = MergeEvent {
        pr_id: pr,
        pr_number: pr,
        merge_sha: format!("sha{pr}"),
        repo: Some("org/eustress".into()),
        title: "Fix".into(),
        github_username: "alice".into(),
        branch: "main".into(),
        merit_points: 50,
    };
    ledger::record_merge(pool, &pay_cfg(), &ev).await.unwrap().unwrap().id
}

/// Make every audit insert fail, the way a full disk or a broken table would.
async fn break_audit_writes(pool: &SqlitePool) {
    sqlx::query("CREATE TRIGGER audit_log_test_refuse BEFORE INSERT ON audit_log BEGIN SELECT RAISE(ABORT, 'audit store unavailable'); END")
        .execute(pool)
        .await
        .unwrap();
}

// ------------------------------------------------------------------ canonical form

#[test]
fn canonical_json_sorts_keys_and_drops_whitespace() {
    let v = json!({"b": 1, "a": {"z": [true, null, "x\"y"], "c": 2.5}, "é": "ü"});
    assert_eq!(canonical_json(&v), r#"{"a":{"c":2.5,"z":[true,null,"x\"y"]},"b":1,"é":"ü"}"#);
}

#[test]
fn entry_hash_is_pinned() {
    // Pins the documented serialization: changing it breaks every stored chain.
    let h = entry_hash(1, "2026-11-21T19:00:00.000Z", "default", "system", "system:test", "test.event", "thing:1", &json!({"n": 1}), GENESIS_HASH);
    let expected_input = format!(
        r#"["msync-audit-v1",1,"2026-11-21T19:00:00.000Z","default","system","system:test","test.event","thing:1",{{"n":1}},"{GENESIS_HASH}"]"#
    );
    assert_eq!(h, hex::encode(Sha256::digest(expected_input.as_bytes())));
}

#[test]
fn redaction_keeps_last_four_and_drops_secrets() {
    assert_eq!(stripe_last4(ACCT), "acct_...sDfG");
    let s = redact_str("No such destination: 'acct_1QwErTyUiOpAsDfG'; key sk_test_51Habc; token msync_0123456789abcdef_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; Bearer abc.def");
    assert!(!s.contains(ACCT), "{s}");
    assert!(s.contains("acct_...sDfG"), "{s}");
    assert!(!s.contains("sk_test_51Habc"), "{s}");
    assert!(s.contains("msync_0123456789abcdef_[redacted]"), "{s}");
    assert!(!s.contains("abc.def"), "{s}");
    assert_eq!(redact_str(&s), s, "idempotent");
    let v = redact_value(&json!({"secret": "hunter2", "nested": [{"token": "t"}], "ok": "fine"}));
    assert_eq!(v, json!({"secret": "[redacted]", "nested": [{"token": "[redacted]"}], "ok": "fine"}));
}

// ------------------------------------------------------------------ append-only

#[tokio::test]
async fn database_refuses_update_and_delete() {
    let pool = pool().await;
    for i in 0..3 {
        append(&pool, &entry(i)).await.unwrap();
    }
    let upd = sqlx::query("UPDATE audit_log SET detail = '{}' WHERE seq = 2").execute(&pool).await;
    let err = upd.expect_err("an UPDATE must be refused");
    assert!(err.to_string().contains("append-only"), "{err}");
    let del = sqlx::query("DELETE FROM audit_log WHERE seq = 3").execute(&pool).await;
    let err = del.expect_err("a DELETE must be refused");
    assert!(err.to_string().contains("append-only"), "{err}");
    // A forged insert that does not extend the head is refused too.
    let forged = sqlx::query(
        "INSERT INTO audit_log (seq, at, org_id, actor_kind, actor, action, target, detail, prev_hash, hash)
         VALUES (2, 'x', 'default', 'system', 'x', 'x', 'x', '{}', ?, 'f00d')",
    )
    .bind(GENESIS_HASH)
    .execute(&pool)
    .await;
    assert!(forged.is_err());
    assert_eq!(count(&pool).await, 3);
    assert!(verify(&pool).await.unwrap().ok);
}

#[tokio::test]
async fn editing_an_entry_out_of_band_is_named_by_verify() {
    let pool = pool().await;
    for i in 0..5 {
        append(&pool, &entry(i)).await.unwrap();
    }
    let before = verify(&pool).await.unwrap();
    assert!(before.ok, "{before:?}");
    assert_eq!(before.checked, 5);
    assert_eq!(before.head_seq, Some(5));

    // Someone with raw database access drops the guard and rewrites entry 3's detail, keeping
    // it canonical so only the hash can tell.
    sqlx::query("DROP TRIGGER audit_log_no_update").execute(&pool).await.unwrap();
    sqlx::query(r#"UPDATE audit_log SET detail = '{"a":[1,2],"b":"x","n":999}' WHERE seq = 3"#).execute(&pool).await.unwrap();

    let after = verify(&pool).await.unwrap();
    assert!(!after.ok);
    let broken = after.first_broken.expect("a broken entry");
    assert_eq!(broken.seq, 3, "{broken:?}");
    assert!(broken.reason.contains("hash"), "{broken:?}");
    assert_eq!(after.checked, 2);
}

#[tokio::test]
async fn removing_an_entry_breaks_the_chain() {
    let pool = pool().await;
    for i in 0..5 {
        append(&pool, &entry(i)).await.unwrap();
    }
    sqlx::query("DROP TRIGGER audit_log_no_delete").execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM audit_log WHERE seq = 3").execute(&pool).await.unwrap();
    let r = verify(&pool).await.unwrap();
    assert!(!r.ok);
    let broken = r.first_broken.expect("a broken entry");
    assert_eq!(broken.seq, 4, "the entry after the gap is the first that does not verify: {broken:?}");
    assert!(broken.reason.contains("removed"), "{broken:?}");
}

#[tokio::test]
async fn fifty_concurrent_appends_make_one_unforked_chain() {
    let path = std::env::temp_dir().join(format!("msync-audit-{}.db", uuid::Uuid::new_v4()));
    let url = format!("sqlite://{}?mode=rwc", path.display().to_string().replace('\\', "/"));
    let opts = SqliteConnectOptions::from_str(&url).unwrap();
    let pool = SqlitePoolOptions::new().max_connections(8).connect_with(opts).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();

    let mut handles = Vec::new();
    for i in 0..50 {
        let p = pool.clone();
        handles.push(tokio::spawn(async move { append(&p, &entry(i)).await }));
    }
    let mut seqs = Vec::new();
    let mut errors = Vec::new();
    for h in handles {
        match h.await.unwrap() {
            Ok(r) => seqs.push(r.seq),
            Err(e) => errors.push(e.to_string()),
        }
    }
    let report = verify(&pool).await.unwrap();
    let rows: Vec<(i64, String, String)> = sqlx::query_as("SELECT seq, prev_hash, hash FROM audit_log ORDER BY seq").fetch_all(&pool).await.unwrap();
    pool.close().await;
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }

    assert!(errors.is_empty(), "every append must succeed: {errors:?}");
    seqs.sort_unstable();
    assert_eq!(seqs, (1..=50).collect::<Vec<i64>>(), "one entry per append, seq 1..=50 with no gap or duplicate");
    assert_eq!(rows.len(), 50);
    for w in rows.windows(2) {
        assert_eq!(w[1].1, w[0].2, "entry {} links to entry {}", w[1].0, w[0].0);
    }
    assert!(report.ok, "{report:?}");
    assert_eq!(report.checked, 50);
}

// ------------------------------------------------------------------ what gets written

#[tokio::test]
async fn strict_actions_write_entries_and_the_chain_verifies() {
    let pool = pool().await;
    let dir = EnvDirectory::new(vec!["owner1".to_string()]);
    let t = create_token(
        &pool,
        &dir,
        TokenCreator::Bootstrap,
        NewToken { label: "ops".into(), member_id: "bob".into(), scopes: vec![scopes::AUDIT_READ.into()], payee_keys: vec![], expires_at: None },
        Utc::now(),
    )
    .await
    .unwrap();
    crate::auth::revoke_token(&pool, &dir, TokenCreator::Bootstrap, &t.id, Utc::now()).await.unwrap();
    crate::auth::revoke_token(&pool, &dir, TokenCreator::Bootstrap, &t.id, Utc::now()).await.unwrap();
    let stripe = MockStripe::default();
    let id = payable_row(&pool, 1).await;
    let paid = ledger::approve(&pool, &stripe, &id, "owner1").await.unwrap();
    assert_eq!(paid.status, PAID);

    let page = list(&pool, &AuditFilter::default()).await.unwrap();
    let actions: Vec<&str> = page.entries.iter().map(|e| e.action.as_str()).collect();
    assert_eq!(actions, vec!["token.created", "token.revoked", "payout.transfer.attempt", "payout.transfer.succeeded"]);
    assert_eq!(page.entries[0].actor, "admin-token");
    assert_eq!(page.entries[0].actor_kind, "admin_token");
    assert_eq!(page.entries[2].detail["destination"], json!("acct_...sDfG"));
    assert_eq!(page.entries[2].detail["idempotency_key"], json!(format!("dsync-ledger-{id}")));

    // Filters and paging.
    let only = list(&pool, &AuditFilter { action: Some("payout.*".into()), ..Default::default() }).await.unwrap();
    assert_eq!(only.entries.len(), 2);
    let p1 = list(&pool, &AuditFilter { limit: Some(3), ..Default::default() }).await.unwrap();
    assert_eq!(p1.entries.len(), 3);
    assert_eq!(p1.next_after, Some(3));
    let p2 = list(&pool, &AuditFilter { limit: Some(3), after: p1.next_after, ..Default::default() }).await.unwrap();
    assert_eq!(p2.entries.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![4]);
    assert_eq!(p2.next_after, None);

    assert!(verify(&pool).await.unwrap().ok);
}

#[tokio::test]
async fn no_secret_or_full_stripe_id_is_ever_stored() {
    let pool = pool().await;
    let dir = EnvDirectory::new(vec!["owner1".to_string()]);
    let t = create_token(
        &pool,
        &dir,
        TokenCreator::Bootstrap,
        NewToken { label: "leaky".into(), member_id: "bob".into(), scopes: vec![scopes::PAYOUTS_READ.into()], payee_keys: vec![], expires_at: None },
        Utc::now(),
    )
    .await
    .unwrap();
    let secret_half = t.token.rsplit('_').next().unwrap().to_string();
    assert_eq!(secret_half.len(), 64);

    let admin = Principal::legacy_admin();
    let payee = crate::payments::payees::create(
        &pool,
        &dir,
        &admin,
        crate::payments::payees::NewPayee {
            display_name: "Acme".into(),
            legal_name: None,
            kind: "company".into(),
            email: None,
            member_id: None,
            payee_keys: vec!["acme".into()],
            stripe_account_id: Some(ACCT.into()),
            tax_form_status: None,
            notes: None,
        },
        Utc::now(),
    )
    .await
    .unwrap();
    assert_eq!(payee.row.stripe_account_id.as_deref(), Some(ACCT));

    // A transfer whose Stripe error echoes the full account id and a key.
    let stripe = MockStripe { fail_with: Some(format!("No such destination: '{ACCT}' (request with sk_test_51HabcDEF)")), ..Default::default() };
    let id = payable_row(&pool, 7).await;
    let failed = ledger::approve(&pool, &stripe, &id, "owner1").await.unwrap();
    assert_eq!(failed.status, ledger::FAILED);

    // An entry built carelessly by a future call site is still redacted.
    append(&pool, &AuditEntry::new(ActorKind::System, "system:test", "test.careless", ACCT, json!({"token": t.token, "note": t.token}))).await.unwrap();

    let all = dump(&pool).await;
    assert!(count(&pool).await >= 5, "{all}");
    assert!(!all.contains(ACCT), "full Stripe account id stored:\n{all}");
    assert!(!all.contains(&secret_half), "token secret stored:\n{all}");
    assert!(!all.contains(&t.token), "token stored:\n{all}");
    assert!(!all.contains("sk_test_51HabcDEF"), "Stripe key stored:\n{all}");
    assert!(all.contains("acct_...sDfG"), "the last four are kept:\n{all}");
    assert!(verify(&pool).await.unwrap().ok);
}

#[tokio::test]
async fn a_strict_action_fails_when_its_audit_entry_cannot_be_written() {
    let pool = pool().await;
    let dir = EnvDirectory::new(vec!["owner1".to_string()]);
    let stripe = MockStripe::default();
    let id = payable_row(&pool, 3).await;
    break_audit_writes(&pool).await;

    // Token creation: refused, and no token row.
    let t = create_token(
        &pool,
        &dir,
        TokenCreator::Bootstrap,
        NewToken { label: "x".into(), member_id: "bob".into(), scopes: vec![scopes::PAYOUTS_READ.into()], payee_keys: vec![], expires_at: None },
        Utc::now(),
    )
    .await;
    assert!(t.is_err(), "token created without its audit entry");
    let tokens: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_tokens").fetch_one(&pool).await.unwrap();
    assert_eq!(tokens, 0);

    // A transfer: refused before Stripe is called, and the row stays claimable.
    let r = ledger::approve(&pool, &stripe, &id, "owner1").await;
    assert!(r.is_err(), "transfer made without its audit entry: {r:?}");
    assert_eq!(stripe.transfers.lock().unwrap().len(), 0, "Stripe was called");
    assert_eq!(ledger::get(&pool, &id).await.unwrap().unwrap().status, PENDING);

    // Policy change: refused, and no stored version.
    let p = crate::payments::policy::PayoutPolicy::from_config(&pay_cfg());
    assert!(crate::payments::policy::put_policy(&pool, &Principal::legacy_admin(), &p, None).await.is_err());
    let versions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payout_policies").fetch_one(&pool).await.unwrap();
    assert_eq!(versions, 0);
    assert_eq!(count(&pool).await, 0);
}

#[tokio::test]
async fn ledger_approval_is_audited_with_the_approver() {
    let pool = pool().await;
    let dir = EnvDirectory::new(vec!["owner1".to_string()]);
    let id = payable_row(&pool, 9).await;
    let t = create_token(
        &pool,
        &dir,
        TokenCreator::Bootstrap,
        NewToken { label: "o".into(), member_id: "owner1".into(), scopes: vec![scopes::PAYOUTS_APPROVE.into()], payee_keys: vec![], expires_at: None },
        Utc::now(),
    )
    .await
    .unwrap();
    let p = crate::auth::authenticate_token(&pool, &t.token, Utc::now()).await.unwrap();
    crate::payments::approvals::approve_row(&pool, &dir, &p, &id).await.unwrap();
    // A second approval by the same member changes nothing and writes nothing.
    crate::payments::approvals::approve_row(&pool, &dir, &p, &id).await.unwrap();
    let e = list(&pool, &AuditFilter { action: Some("ledger.approval".into()), ..Default::default() }).await.unwrap();
    assert_eq!(e.entries.len(), 1);
    assert_eq!(e.entries[0].actor, "owner1");
    assert_eq!(e.entries[0].target, format!("ledger:{id}"));
    assert_eq!(e.entries[0].detail["token_id"], json!(t.id));
}
