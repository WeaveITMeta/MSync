//! MCP and v1 merit REST over HTTP (`actix_web::test`). Stripe is a local mock; nothing here
//! calls a network service.

use super::tools::{EXECUTE_TOOL, TOOLS};
use super::*;
use crate::auth::{create_token, scopes, EnvDirectory, NewToken, TokenCreator};
use crate::foundry::gitlab::tests::mr_hook;
use crate::foundry::{github, gitlab, FoundryProcessor, RepoRegistry};
use crate::github::webhook::GitHubWebhookPayload;
use crate::merit::api::{self as merit_api, AppStateSeams, FixedSeams, MeritContext};
use crate::merit::rubric::{CRITERIA, RUBRIC_ID, RUBRIC_SHA256};
use crate::merit::scoring::MeritRules;
use crate::payments::ledger::{self, MergeEvent};
use crate::payments::{PaymentConfig, StripeApi, TransferRequest};
use actix_web::http::StatusCode;
use actix_web::{test as atest, web, App};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::sync::{Arc, Mutex};

const SPEC: &str = "gitlab:gitlab.com:grp/proj, github:github.com:org/alpha, github:github.com:org/secret-sauce:private";

#[derive(Default)]
struct MockStripe {
    transfers: Mutex<Vec<TransferRequest>>,
}

#[async_trait]
impl StripeApi for MockStripe {
    async fn create_express_account(&self, _u: &str) -> anyhow::Result<String> {
        Ok("acct_mock".into())
    }
    async fn create_account_link(&self, a: &str, _r: &str, _ret: &str) -> anyhow::Result<String> {
        Ok(format!("https://connect.stripe.test/{a}"))
    }
    async fn create_transfer(&self, req: &TransferRequest) -> anyhow::Result<String> {
        self.transfers.lock().unwrap().push(req.clone());
        Ok(format!("tr_mock_{}", self.transfers.lock().unwrap().len()))
    }
}

fn cfg() -> PaymentConfig {
    PaymentConfig::from_lookup(|k| match k {
        "MSYNC_PAY_BRANCHES" => Some("main".into()),
        "MSYNC_PAY_BASE_CENTS_MAIN" => Some("2000".into()),
        "MSYNC_PAY_MERIT_MULTIPLIER" => Some("0.01".into()),
        "MSYNC_PAY_MAX_CENTS" => Some("5000".into()),
        _ => None,
    })
}

async fn pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    pool
}

fn registry() -> Arc<RepoRegistry> {
    Arc::new(RepoRegistry::from_spec(SPEC).unwrap())
}

fn ctx(pool: &SqlitePool, stripe: Arc<MockStripe>, allow_execute: bool) -> McpContext {
    let api = ApiContext {
        pool: pool.clone(),
        payments: Arc::new(cfg()),
        stripe,
        directory: Arc::new(EnvDirectory::new(Vec::<String>::new())),
    };
    let merit = MeritContext {
        pool: pool.clone(),
        seams: Arc::new(FixedSeams {
            registry: registry(),
            rules: Arc::new(MeritRules::default()),
        }),
    };
    McpContext::new(api, merit, allow_execute)
}

macro_rules! app {
    ($ctx:expr) => {{
        let c: McpContext = $ctx;
        atest::init_service(
            App::new()
                .app_data(web::Data::new(c.merit.clone()))
                .app_data(web::Data::new(c))
                .configure(configure)
                .configure(merit_api::configure),
        )
        .await
    }};
}

fn to_json(bytes: &[u8]) -> Value {
    if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(bytes).into_owned()))
    }
}

/// POST /mcp with an optional bearer token and a raw body. Returns (status, json body).
macro_rules! post_mcp {
    ($app:expr, $token:expr, $body:expr) => {{
        let tok: Option<&str> = $token;
        let mut req = atest::TestRequest::post()
            .uri("/mcp")
            .insert_header(("Content-Type", "application/json"));
        if let Some(t) = tok {
            req = req.insert_header(("Authorization", format!("Bearer {}", t)));
        }
        let resp =
            atest::call_service(&$app, req.set_payload(($body).to_string()).to_request()).await;
        let status = resp.status();
        let bytes = atest::read_body(resp).await;
        (status, to_json(&bytes))
    }};
}

macro_rules! rpc {
    ($app:expr, $token:expr, $method:expr, $params:expr) => {
        post_mcp!($app, Some($token), json!({"jsonrpc": "2.0", "id": 1, "method": $method, "params": $params}))
    };
}

/// tools/call; asserts HTTP 200 and returns the JSON-RPC response.
macro_rules! tool {
    ($app:expr, $token:expr, $name:expr, $args:expr) => {{
        let (s, v) = rpc!($app, $token, "tools/call", json!({"name": $name, "arguments": $args}));
        assert_eq!(s, StatusCode::OK, "{v}");
        v
    }};
}

macro_rules! rest {
    ($app:expr, $req:expr) => {{
        let resp = atest::call_service(&$app, $req.to_request()).await;
        let status = resp.status();
        let bytes = atest::read_body(resp).await;
        (status, to_json(&bytes))
    }};
}

fn is_error(v: &Value) -> bool {
    v["result"]["isError"]
        .as_bool()
        .unwrap_or_else(|| panic!("not a tool result: {v}"))
}

fn text(v: &Value) -> String {
    v["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn structured(v: &Value) -> &Value {
    &v["result"]["structuredContent"]
}

fn listed(v: &Value) -> Vec<String> {
    v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

async fn token(pool: &SqlitePool, member: &str, scopes: &[&str], payee: &[&str]) -> String {
    create_token(
        pool,
        &EnvDirectory::default(),
        TokenCreator::Bootstrap,
        NewToken {
            label: format!("{member} test token"),
            member_id: member.into(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            payee_keys: payee.iter().map(|s| s.to_string()).collect(),
            expires_at: None,
        },
        Utc::now(),
    )
    .await
    .unwrap()
    .token
}

/// Open (and optionally merge) GitLab MR id 555 in grp/proj authored by carol.
async fn seed_mr(pool: &SqlitePool, merge: bool) {
    let p = FoundryProcessor::new(pool.clone())
        .with_registry(registry())
        .with_rules(Arc::new(MeritRules::default()));
    let actions: &[&str] = if merge { &["open", "merge"] } else { &["open"] };
    for action in actions {
        let payload: gitlab::GitLabWebhookPayload = serde_json::from_value(mr_hook(
            action,
            (5, "carol"),
            5,
            "grp/proj",
            "https://gitlab.com/grp/proj",
        ))
        .unwrap();
        let ev = gitlab::normalize(&payload, None).unwrap().unwrap();
        p.process(&ev).await.unwrap();
    }
}

/// Open + merge a GitHub PR by `login` in `repo`.
async fn seed_gh_pr(pool: &SqlitePool, repo: &str, id: i64, login: &str) {
    let p = FoundryProcessor::new(pool.clone())
        .with_registry(registry())
        .with_rules(Arc::new(MeritRules::default()));
    for (action, merged) in [("opened", false), ("closed", true)] {
        let payload: GitHubWebhookPayload = serde_json::from_value(json!({
            "action": action, "repository": {"full_name": repo},
            "pull_request": {"id": id, "number": 1, "title": "T", "body": null, "state": "x", "merged": merged,
                "user": {"login": login}, "merge_commit_sha": "abc", "base": {"ref": "main"}}
        }))
        .unwrap();
        let ev = github::normalize("pull_request", &payload, None).unwrap();
        p.process(&ev).await.unwrap();
    }
}

fn subject() -> Value {
    json!({"provider": "gitlab", "repo": "grp/proj", "subject_id": "555"})
}

fn score_args(levels: [u8; 6]) -> Value {
    let mut criteria = serde_json::Map::new();
    for (c, l) in CRITERIA.iter().zip(levels) {
        criteria.insert(c.key.into(), json!({"level": l, "evidence": format!("checked {} against the diff and tests", c.key)}));
    }
    json!({
        "subject": subject(),
        "rubric_id": RUBRIC_ID,
        "rubric_hash": RUBRIC_SHA256,
        "ci_passed": true,
        "reverted": false,
        "criteria": criteria,
    })
}

async fn count(pool: &SqlitePool, table: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn add_payee(pool: &SqlitePool, gh: &str) {
    sqlx::query(
        "INSERT INTO contributors (id, github_username, discord_user_id, created_at, updated_at, stripe_account_id, stripe_payouts_enabled)
         VALUES (?, ?, 42, datetime('now'), datetime('now'), ?, 1)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(gh)
    .bind(format!("acct_{gh}"))
    .execute(pool)
    .await
    .unwrap();
}

async fn ledger_row(pool: &SqlitePool, pr_id: i64, user: &str) -> String {
    let ev = MergeEvent {
        pr_id,
        pr_number: pr_id,
        merge_sha: format!("sha{pr_id}"),
        repo: Some("org/alpha".into()),
        title: "Fix thing".into(),
        github_username: user.into(),
        branch: "main".into(),
        merit_points: 50, // 2000 * 1.5 = 3000 cents
    };
    ledger::record_merge(pool, &cfg(), &ev)
        .await
        .unwrap()
        .unwrap()
        .id
}

const ALL_TOOL_NAMES: &[&str] = &[
    "rubric_get",
    "merit_score_submit",
    "merit_scores_list",
    "leaderboard_get",
    "member_merit_get",
    "payouts_policy_get",
    "payouts_ledger_list",
    "payouts_run_preview",
    "payouts_run_create",
    "payouts_run_approve",
    "payouts_run_execute",
    "payees_list",
    "payee_get",
    "payorders_list",
    "payorder_preview",
    "payorder_create",
    "payorder_approve",
    "payorder_pause",
    "invoices_list",
    "invoice_submit",
    "invoice_approve",
    "payout_statement",
    "audit_list",
    "audit_verify",
    "time_clock_in",
    "time_entries_list",
    "time_shifts_list",
    "time_timesheet_get",
    "time_entry_approve",
];

// ------------------------------------------------------------------ auth

#[actix_web::test]
async fn no_token_and_bad_tokens_get_the_rest_401() {
    let pool = pool().await;
    let good = token(&pool, "bob", &[scopes::MERIT_READ], &[]).await;
    let revoked = token(&pool, "carol", &[scopes::MERIT_READ], &[]).await;
    sqlx::query("UPDATE api_tokens SET revoked_at = datetime('now') WHERE member_id = 'carol'")
        .execute(&pool)
        .await
        .unwrap();
    let (id, _) = crate::auth::tokens::parse_token(&good).unwrap();
    let wrong_secret = format!("msync_{id}_{}", "f".repeat(64));
    let app = app!(ctx(&pool, Arc::new(MockStripe::default()), true));

    // The REST 401 body, from a v1 REST route.
    let (s, rest_body) = rest!(app, atest::TestRequest::get().uri("/api/v1/leaderboard"));
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(
        rest_body,
        json!({"status": "error", "message": "unauthorized"})
    );

    let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    for (tok, body) in [
        (None, list.to_string()),
        (None, "{not json".to_string()), // auth is checked before the body is parsed
        (Some("nonsense"), list.to_string()),
        (Some(wrong_secret.as_str()), list.to_string()),
        (Some(revoked.as_str()), list.to_string()),
    ] {
        let (s, v) = post_mcp!(app, tok, body);
        assert_eq!(s, StatusCode::UNAUTHORIZED, "token {tok:?}");
        assert_eq!(v, rest_body, "same body as REST for token {tok:?}");
    }

    // Control: the good token gets through.
    let (s, v) = post_mcp!(app, Some(good.as_str()), list.to_string());
    assert_eq!(s, StatusCode::OK);
    assert!(v["result"]["tools"].is_array(), "{v}");
}

#[actix_web::test]
async fn every_tool_enforces_its_scope_in_list_and_call() {
    let pool = pool().await;
    let stripe = Arc::new(MockStripe::default());
    let app = app!(ctx(&pool, stripe.clone(), true));

    // The table covers every tool, and the tool set is pinned.
    let names: Vec<&str> = TOOLS.iter().map(|t| t.name).collect();
    assert_eq!(names, ALL_TOOL_NAMES);

    // Something real for the payout tools to act on, so a missing check would have an effect.
    add_payee(&pool, "alice").await;
    ledger_row(&pool, 1, "alice").await;
    seed_mr(&pool, true).await;

    let mut checked = 0;
    for t in TOOLS {
        let Some(scope) = t.scope else {
            // No scope beyond a valid token: a token with one unrelated scope sees and calls it.
            let tok = token(
                &pool,
                &format!("m-{}", t.name),
                &[scopes::PAYOUTS_READ],
                &[],
            )
            .await;
            let (_, v) = rpc!(app, &tok, "tools/list", json!({}));
            assert!(
                listed(&v).contains(&t.name.to_string()),
                "{} must be listed for any token",
                t.name
            );
            let v = tool!(app, &tok, t.name, json!({}));
            assert!(!is_error(&v), "{v}");
            checked += 1;
            continue;
        };
        let others: Vec<&str> = scopes::ALL
            .iter()
            .copied()
            .filter(|s| *s != scope)
            .collect();
        let tok = token(&pool, &format!("m-{}", t.name), &others, &[]).await;
        let (_, v) = rpc!(app, &tok, "tools/list", json!({}));
        let visible = listed(&v);
        assert!(
            !visible.contains(&t.name.to_string()),
            "{} listed without {scope}",
            t.name
        );
        let expected: Vec<String> = TOOLS
            .iter()
            .filter(|o| o.scope != Some(scope))
            .map(|o| o.name.to_string())
            .collect();
        assert_eq!(
            visible, expected,
            "every tool not needing {scope} stays visible"
        );

        let v = tool!(app, &tok, t.name, json!({}));
        assert!(is_error(&v), "{} callable without {scope}: {v}", t.name);
        assert_eq!(
            text(&v),
            format!(
                "tool '{}' requires scope '{}', which this token does not have",
                t.name, scope
            )
        );
        checked += 1;
    }
    assert_eq!(checked, TOOLS.len());

    // Nothing happened: no score, run, approval or transfer.
    assert_eq!(count(&pool, "merit_scores").await, 0);
    assert_eq!(count(&pool, "payout_runs").await, 0);
    assert_eq!(count(&pool, "payout_approvals").await, 0);
    assert_eq!(stripe.transfers.lock().unwrap().len(), 0);

    // Control: a token with every scope sees every tool.
    let all = token(&pool, "root", scopes::ALL, &[]).await;
    let (_, v) = rpc!(app, &all, "tools/list", json!({}));
    assert_eq!(listed(&v), ALL_TOOL_NAMES);
    let tools = v["result"]["tools"].as_array().unwrap();
    for t in tools {
        assert_eq!(t["inputSchema"]["type"], "object", "{t}");
        assert!(t["description"].as_str().unwrap().len() > 40, "{t}");
    }
}

// ------------------------------------------------------------------ payouts

#[actix_web::test]
async fn execute_is_refused_and_unlisted_unless_enabled_then_pays_through_mock_stripe() {
    let pool = pool().await;
    let stripe = Arc::new(MockStripe::default());
    add_payee(&pool, "alice").await;
    let row = ledger_row(&pool, 1, "alice").await;
    let preparer = token(&pool, "prep", &[scopes::PAYOUTS_PREPARE], &[]).await;
    let approver = token(&pool, "bob", &[scopes::PAYOUTS_APPROVE], &[]).await;
    let executor = token(
        &pool,
        "exec",
        &[scopes::PAYOUTS_EXECUTE, scopes::PAYOUTS_READ],
        &[],
    )
    .await;

    let off = app!(ctx(&pool, stripe.clone(), false));
    let v = tool!(off, &preparer, "payouts_run_create", json!({}));
    assert!(!is_error(&v), "{v}");
    let run_id = structured(&v)["run"]["run"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(structured(&v)["run"]["run"]["created_by"], "prep");
    let v = tool!(
        off,
        &approver,
        "payouts_run_approve",
        json!({"run_id": run_id})
    );
    assert_eq!(
        structured(&v)["report"]["approved"],
        json!([row.clone()]),
        "{v}"
    );

    // Flag off: not listed, and a call is refused with the operator instruction.
    let (_, v) = rpc!(off, &executor, "tools/list", json!({}));
    assert!(!listed(&v).contains(&EXECUTE_TOOL.to_string()));
    let all = token(&pool, "root", scopes::ALL, &[]).await;
    let (_, v) = rpc!(off, &all, "tools/list", json!({}));
    assert!(
        !listed(&v).contains(&EXECUTE_TOOL.to_string()),
        "not listed even with every scope"
    );
    let v = tool!(off, &executor, EXECUTE_TOOL, json!({"run_id": run_id}));
    assert!(is_error(&v));
    assert!(
        text(&v).contains("MSYNC_MCP_ALLOW_EXECUTE=true") && text(&v).contains("operator"),
        "{}",
        text(&v)
    );
    assert_eq!(stripe.transfers.lock().unwrap().len(), 0);
    let v = tool!(
        off,
        &executor,
        "payouts_ledger_list",
        json!({"status": "pending_approval"})
    );
    assert_eq!(
        structured(&v)["entries"].as_array().unwrap().len(),
        1,
        "row still pending"
    );

    // Flag on: listed, and the run pays through the existing transfer path (mock Stripe).
    let on = app!(ctx(&pool, stripe.clone(), true));
    let (_, v) = rpc!(on, &executor, "tools/list", json!({}));
    assert!(listed(&v).contains(&EXECUTE_TOOL.to_string()));
    let v = tool!(on, &executor, EXECUTE_TOOL, json!({"run_id": run_id}));
    assert!(!is_error(&v), "{v}");
    let report = &structured(&v)["report"];
    assert_eq!(report["paid_cents"], 3000, "{report}");
    assert_eq!(report["items"][0]["outcome"], "paid");
    let transfers = stripe.transfers.lock().unwrap().clone();
    assert_eq!(transfers.len(), 1);
    assert_eq!(transfers[0].idempotency_key, format!("dsync-ledger-{row}"));
    assert_eq!(
        ledger::get(&pool, &row).await.unwrap().unwrap().status,
        ledger::PAID
    );

    // Safe to repeat: no second transfer.
    let v = tool!(on, &executor, EXECUTE_TOOL, json!({"run_id": run_id}));
    assert_eq!(
        structured(&v)["report"]["items"][0]["outcome"],
        "already_paid",
        "{v}"
    );
    assert_eq!(stripe.transfers.lock().unwrap().len(), 1);
}

#[actix_web::test]
async fn payee_cannot_approve_their_own_row_through_mcp() {
    let pool = pool().await;
    add_payee(&pool, "carol").await;
    let row = ledger_row(&pool, 7, "carol").await;
    let preparer = token(&pool, "prep", &[scopes::PAYOUTS_PREPARE], &[]).await;
    let carol = token(&pool, "carol", &[scopes::PAYOUTS_APPROVE], &["carol"]).await;
    // A second token of the same member that declares no payee key: still the payee.
    let carol_bare = token(&pool, "carol", &[scopes::PAYOUTS_APPROVE], &[]).await;
    let app = app!(ctx(&pool, Arc::new(MockStripe::default()), true));

    let v = tool!(app, &preparer, "payouts_run_create", json!({}));
    let run_id = structured(&v)["run"]["run"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    for tok in [&carol, &carol_bare] {
        let v = tool!(app, tok, "payouts_run_approve", json!({"run_id": run_id}));
        assert!(!is_error(&v), "{v}");
        let report = &structured(&v)["report"];
        assert_eq!(report["approver_member_id"], "carol");
        assert_eq!(report["skipped_payee"], json!([row.clone()]), "{report}");
        assert_eq!(report["approved"], json!([]), "{report}");
    }
    assert_eq!(
        count(&pool, "payout_approvals").await,
        0,
        "no approval recorded for the payee"
    );

    // Control: a different member's approval is recorded, as that member.
    let bob = token(&pool, "bob", &[scopes::PAYOUTS_APPROVE], &[]).await;
    let v = tool!(app, &bob, "payouts_run_approve", json!({"run_id": run_id}));
    assert_eq!(structured(&v)["report"]["approved"], json!([row]));
    let approvers: Vec<String> =
        sqlx::query_scalar("SELECT approver_member_id FROM payout_approvals")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(approvers, vec!["bob".to_string()]);
}

#[actix_web::test]
async fn inputs_naming_an_actor_or_approver_are_rejected() {
    let pool = pool().await;
    seed_mr(&pool, false).await;
    add_payee(&pool, "alice").await;
    ledger_row(&pool, 1, "alice").await;
    let all = token(&pool, "bob", scopes::ALL, &[]).await;
    let app = app!(ctx(&pool, Arc::new(MockStripe::default()), true));

    for (name, args) in [
        ("payouts_run_create", json!({"created_by": "mallory"})),
        ("payouts_run_preview", json!({"actor": "mallory"})),
        (
            "payouts_run_approve",
            json!({"run_id": "x", "approver_member_id": "mallory"}),
        ),
        (EXECUTE_TOOL, json!({"run_id": "x", "member_id": "mallory"})),
        ("merit_score_submit", {
            let mut a = score_args([3, 3, 3, 3, 3, 3]);
            a["scorer_id"] = json!("mallory");
            a
        }),
        ("merit_score_submit", {
            let mut a = score_args([3, 3, 3, 3, 3, 3]);
            a["subject"]["author"] = json!("mallory");
            a
        }),
    ] {
        let (s, v) = rpc!(
            app,
            &all,
            "tools/call",
            json!({"name": name, "arguments": args})
        );
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["error"]["code"], -32602, "{name}: {v}");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("unknown field"),
            "{v}"
        );
    }
    assert_eq!(count(&pool, "payout_runs").await, 0);
    assert_eq!(count(&pool, "merit_scores").await, 0);

    // REST rejects the same field.
    let mut body = score_args([3, 3, 3, 3, 3, 3]);
    body["scorer_id"] = json!("mallory");
    let (s, v) = rest!(
        app,
        atest::TestRequest::post()
            .uri("/api/v1/merit/scores")
            .insert_header(("Authorization", format!("Bearer {all}")))
            .set_json(&body)
    );
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(count(&pool, "merit_scores").await, 0);

    // Control: without the extra field the actor is the token's member.
    let v = tool!(app, &all, "payouts_run_create", json!({}));
    assert_eq!(structured(&v)["run"]["run"]["created_by"], "bob", "{v}");
    let v = tool!(
        app,
        &all,
        "merit_score_submit",
        score_args([3, 3, 3, 3, 3, 3])
    );
    assert_eq!(structured(&v)["score"]["scorer_id"], "bob", "{v}");
}

// ------------------------------------------------------------------ merit

#[actix_web::test]
async fn self_score_is_refused_when_the_token_declares_the_author_login() {
    let pool = pool().await;
    seed_mr(&pool, false).await; // authored by gitlab carol
    let app = app!(ctx(&pool, Arc::new(MockStripe::default()), true));

    for keys in [&["gitlab:carol"][..], &["gitlab:gitlab.com:Carol"][..]] {
        let tok = token(
            &pool,
            &format!("carol-{}", keys[0].len()),
            &[scopes::MERIT_SCORE],
            keys,
        )
        .await;
        let v = tool!(
            app,
            &tok,
            "merit_score_submit",
            score_args([4, 4, 4, 4, 4, 4])
        );
        assert!(is_error(&v), "{keys:?}: {v}");
        assert_eq!(text(&v), "a scorer may not score work they authored");
    }
    assert_eq!(count(&pool, "merit_scores").await, 0);

    // A GitHub key with the same name is a different identity: accepted.
    let dave = token(&pool, "dave", &[scopes::MERIT_SCORE], &["carol"]).await;
    let v = tool!(
        app,
        &dave,
        "merit_score_submit",
        score_args([4, 4, 4, 4, 4, 4])
    );
    assert!(!is_error(&v), "{v}");
    assert_eq!(count(&pool, "merit_scores").await, 1);
}

/// KNOWN LIMIT, pinned on purpose. Self-score detection uses only the GitHub and GitLab logins derived from
/// the token's payee keys. The author's own token with no payee keys is NOT recognized as the
/// author and the score is accepted. A later round supplies identities from the CRA; when it
/// lands, this test must be changed deliberately to expect a refusal.
#[actix_web::test]
async fn known_limit_scorer_without_payee_keys_is_not_recognized_as_author() {
    let pool = pool().await;
    seed_mr(&pool, false).await; // authored by gitlab carol
    let carol = token(&pool, "carol", &[scopes::MERIT_SCORE], &[]).await;
    let app = app!(ctx(&pool, Arc::new(MockStripe::default()), true));
    let v = tool!(
        app,
        &carol,
        "merit_score_submit",
        score_args([4, 4, 4, 4, 4, 4])
    );
    assert!(!is_error(&v), "today a keyless author is accepted: {v}");
    assert_eq!(structured(&v)["score"]["scorer_id"], "carol");
    assert_eq!(count(&pool, "merit_scores").await, 1);
}

#[actix_web::test]
async fn mcp_and_rest_return_the_same_score_for_the_same_submission() {
    let pool = pool().await;
    seed_mr(&pool, true).await; // merged: base 10 points, adjustable once
    let a = token(
        &pool,
        "agent-a",
        &[scopes::MERIT_SCORE, scopes::MERIT_READ],
        &[],
    )
    .await;
    let b = token(
        &pool,
        "agent-b",
        &[scopes::MERIT_SCORE, scopes::MERIT_READ],
        &[],
    )
    .await;
    let app = app!(ctx(&pool, Arc::new(MockStripe::default()), true));
    // impact 3 (22.5) + correctness 1 (5) + difficulty 1 (3.75) = 31.25 -> 31.3
    let args = score_args([3, 1, 1, 0, 0, 0]);

    let v = tool!(app, &a, "merit_score_submit", args.clone());
    assert!(!is_error(&v), "{v}");
    let mcp = structured(&v).clone();
    // The text block carries the same JSON as structuredContent.
    assert_eq!(serde_json::from_str::<Value>(&text(&v)).unwrap(), mcp);

    let (s, rest_v) = rest!(
        app,
        atest::TestRequest::post()
            .uri("/api/v1/merit/scores")
            .insert_header(("Authorization", format!("Bearer {b}")))
            .set_json(&args)
    );
    assert_eq!(s, StatusCode::CREATED, "{rest_v}");
    let rest_r = &rest_v["result"];

    assert_eq!(mcp["score"]["score"], json!(31.3));
    assert_eq!(mcp["score"]["score"], rest_r["score"]["score"]);
    for k in [
        "rubric_id",
        "rubric_hash",
        "subject",
        "ci_passed",
        "reverted",
        "detail",
        "scorer_source",
    ] {
        assert_eq!(mcp["score"][k], rest_r["score"][k], "{k}");
    }
    assert_eq!(mcp["score"]["scorer_id"], "agent-a");
    assert_eq!(rest_r["score"]["scorer_id"], "agent-b");
    // The first score re-priced the merge once (10 -> 26 points); the second did not.
    assert_eq!(
        mcp["merit_adjustment"],
        json!({"score": 31.3, "old_points": 10, "new_points": 26, "delta": 16})
    );
    assert_eq!(rest_r["merit_adjustment"], Value::Null);

    // Listing agrees across the two surfaces.
    let v = tool!(app, &a, "merit_scores_list", subject());
    let (s, rest_list) = rest!(
        app,
        atest::TestRequest::get()
            .uri("/api/v1/merit/scores?provider=gitlab&repo=grp/proj&subject_id=555")
            .insert_header(("Authorization", format!("Bearer {a}")))
    );
    assert_eq!(s, StatusCode::OK, "{rest_list}");
    assert_eq!(structured(&v), &rest_list["result"]);
    assert_eq!(
        rest_list["result"]["aggregate"],
        json!({"score": 31.3, "counted_scorers": 2, "min_scorers": 1})
    );

    // Rubric: public over REST, the same document over MCP.
    let (s, rubric) = rest!(app, atest::TestRequest::get().uri("/api/v1/merit/rubric"));
    assert_eq!(s, StatusCode::OK);
    assert_eq!(rubric["rubric"]["hash"], RUBRIC_SHA256);
    let v = tool!(app, &a, "rubric_get", json!({}));
    assert_eq!(structured(&v), &rubric["rubric"]);
}

#[actix_web::test]
async fn public_leaderboard_hides_private_repo_while_v1_shows_it() {
    let pool = pool().await;
    seed_gh_pr(&pool, "org/alpha", 101, "alice").await;
    seed_gh_pr(&pool, "org/secret-sauce", 102, "alice").await;
    let reader = token(&pool, "bob", &[scopes::MERIT_READ], &[]).await;
    let payer = token(&pool, "pat", &[scopes::PAYOUTS_READ], &[]).await;

    // Real app state and the production seams (routes::foundry_registry), as main wires them.
    let config: crate::config::Config = serde_json::from_value(json!({
        "discord": {"token": "x", "application_id": 1},
        "github": {"token": "x", "webhook_secret": "s", "repo_owner": "o", "repo_name": "r"},
        "coderabbit": {"api_key": null},
        "foundry_repos": serde_json::to_value(&*registry()).unwrap(),
        "database_url": "sqlite::memory:",
        "server": {"host": "127.0.0.1", "port": 0}
    }))
    .unwrap();
    let stripe: Arc<dyn StripeApi> = Arc::new(MockStripe::default());
    let state = web::Data::new(crate::AppState {
        config,
        db_pool: pool.clone(),
        merit_tracker: Arc::new(tokio::sync::RwLock::new(
            crate::merit::tracker::MeritTracker::new(pool.clone()),
        )),
        discord_http: Arc::new(serenity::http::Http::new("x")),
        payments: Arc::new(cfg()),
        stripe,
        directory: Arc::new(EnvDirectory::default()),
        // No active CRA snapshot: registry and merit rules fall back to config, as before.
        cra: {
            let handle = crate::cra::CraHandle::new();
            let dir: Arc<dyn crate::auth::Directory> = Arc::new(EnvDirectory::default());
            Arc::new(crate::cra::CraContext::disabled(pool.clone(), handle, dir))
        },
    });
    let merit = MeritContext {
        pool: pool.clone(),
        seams: Arc::new(AppStateSeams(state.clone())),
    };
    let app = atest::init_service(
        App::new()
            .app_data(state.clone())
            .app_data(web::Data::new(merit))
            .service(crate::routes::get_leaderboard)
            .configure(merit_api::configure),
    )
    .await;

    let (s, public) = rest!(app, atest::TestRequest::get().uri("/api/leaderboard"));
    assert_eq!(s, StatusCode::OK);
    let public_s = public.to_string();
    assert!(
        !public_s.contains("secret-sauce"),
        "public leaderboard names a private repo: {public_s}"
    );
    assert!(
        public_s.contains("private repositories") && public_s.contains("org/alpha"),
        "{public_s}"
    );

    let (s, v1) = rest!(
        app,
        atest::TestRequest::get()
            .uri("/api/v1/leaderboard")
            .insert_header(("Authorization", format!("Bearer {reader}")))
    );
    assert_eq!(s, StatusCode::OK, "{v1}");
    let v1_s = v1.to_string();
    assert!(
        v1_s.contains("secret-sauce") && v1_s.contains("org/alpha"),
        "{v1_s}"
    );
    assert!(!v1_s.contains("private repositories"), "{v1_s}");
    let row = &v1["result"]["leaderboard"][0];
    let sum: i64 = row["by_repo"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["points"].as_i64().unwrap())
        .sum();
    assert_eq!(
        sum,
        row["total_points"].as_i64().unwrap(),
        "drilldown adds up"
    );

    // One private repository alone, by name.
    let (s, one) = rest!(
        app,
        atest::TestRequest::get()
            .uri("/api/v1/leaderboard?provider=github&repo=org/secret-sauce&limit=5")
            .insert_header(("Authorization", format!("Bearer {reader}")))
    );
    assert_eq!(s, StatusCode::OK, "{one}");
    assert_eq!(one["result"]["leaderboard"][0]["total_points"], 15, "{one}");
    assert_eq!(
        one["result"]["leaderboard"][0]["by_repo"][0]["repo"],
        "org/secret-sauce"
    );

    // merit.read is required; a token without it is 403, no token 401.
    let (s, _) = rest!(
        app,
        atest::TestRequest::get()
            .uri("/api/v1/leaderboard")
            .insert_header(("Authorization", format!("Bearer {payer}")))
    );
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = rest!(
        app,
        atest::TestRequest::get().uri("/api/v1/leaderboard?repo=org/x")
    );
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, v) = rest!(
        app,
        atest::TestRequest::get()
            .uri("/api/v1/leaderboard?repo=org/x")
            .insert_header(("Authorization", format!("Bearer {reader}")))
    );
    assert_eq!(s, StatusCode::BAD_REQUEST, "repo without provider: {v}");
    let (s, _) = rest!(
        app,
        atest::TestRequest::get()
            .uri("/api/v1/leaderboard?limit=0")
            .insert_header(("Authorization", format!("Bearer {reader}")))
    );
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Member merit: by repository and by kind, private names included.
    let cid = row["contributor_id"].as_str().unwrap();
    let (s, m) = rest!(
        app,
        atest::TestRequest::get()
            .uri(&format!("/api/v1/members/{cid}/merit"))
            .insert_header(("Authorization", format!("Bearer {reader}")))
    );
    assert_eq!(s, StatusCode::OK, "{m}");
    assert!(m.to_string().contains("secret-sauce"));
    assert_eq!(m["result"]["total_points"], row["total_points"]);
    let (s, _) = rest!(
        app,
        atest::TestRequest::get()
            .uri("/api/v1/members/not-a-uuid/merit")
            .insert_header(("Authorization", format!("Bearer {reader}")))
    );
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = rest!(
        app,
        atest::TestRequest::get()
            .uri(&format!("/api/v1/members/{}/merit", uuid::Uuid::new_v4()))
            .insert_header(("Authorization", format!("Bearer {reader}")))
    );
    assert_eq!(s, StatusCode::NOT_FOUND);

    // MCP leaderboard_get and member_merit_get return the same documents as REST.
    let mcp_app = app!(ctx(&pool, Arc::new(MockStripe::default()), false));
    let v = tool!(mcp_app, &reader, "leaderboard_get", json!({}));
    assert_eq!(structured(&v), &v1["result"]);
    let v = tool!(
        mcp_app,
        &reader,
        "member_merit_get",
        json!({"contributor_id": cid})
    );
    assert_eq!(structured(&v), &m["result"]);
}

// ------------------------------------------------------------------ errors and framing

#[actix_web::test]
async fn tool_errors_never_contain_sql() {
    let pool = pool().await;
    let all = token(&pool, "bob", scopes::ALL, &[]).await;
    let app = app!(ctx(&pool, Arc::new(MockStripe::default()), true));
    // Break the database under the tools.
    for t in [
        "merit_scores",
        "payout_run_items",
        "payout_approvals",
        "payout_runs",
        "payout_ledger",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE {t}")))
            .execute(&pool)
            .await
            .unwrap();
    }

    let forbidden = [
        "select",
        "insert",
        "sqlite",
        "no such table",
        "merit_scores",
        "payout_",
        "database",
        "error returned",
        "\\",
        ".rs",
    ];
    for (name, args) in [
        ("merit_scores_list", subject()),
        ("payouts_ledger_list", json!({})),
        ("payouts_run_preview", json!({})),
        ("payouts_run_approve", json!({"run_id": "r1"})),
        (EXECUTE_TOOL, json!({"run_id": "r1"})),
    ] {
        let v = tool!(app, &all, name, args);
        assert!(is_error(&v), "{name}: {v}");
        let whole = v.to_string().to_lowercase();
        for f in forbidden {
            assert!(!whole.contains(f), "{name} leaked '{f}': {whole}");
        }
        assert_eq!(text(&v), "internal error", "{name}");
    }

    // The REST adapter over the same function is just as quiet.
    let (s, v) = rest!(
        app,
        atest::TestRequest::get()
            .uri("/api/v1/merit/scores?provider=gitlab&repo=grp/proj&subject_id=555")
            .insert_header(("Authorization", format!("Bearer {all}")))
    );
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(v, json!({"status": "error", "message": "internal error"}));
}

#[actix_web::test]
async fn malformed_json_rpc_gets_the_right_error_codes() {
    let pool = pool().await;
    let tok = token(&pool, "bob", &[scopes::MERIT_READ], &[]).await;
    let app = app!(ctx(&pool, Arc::new(MockStripe::default()), false));
    let t = Some(tok.as_str());

    for (body, status, code) in [
        ("{not json", 400, -32700),
        ("", 400, -32700),
        (r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#, 400, -32600),
        (r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#, 400, -32600),
        (r#"{"jsonrpc":"2.0","id":1}"#, 400, -32600),
        (
            r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#,
            400,
            -32600,
        ),
        (
            r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":"x"}"#,
            400,
            -32600,
        ),
        (
            r#"{"jsonrpc":"2.0","id":1,"method":"resources/list"}"#,
            200,
            -32601,
        ),
        (
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            200,
            -32602,
        ),
        (
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}"#,
            200,
            -32602,
        ),
        (
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"nope"}}"#,
            200,
            -32602,
        ),
        (
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"rubric_get","arguments":[1]}}"#,
            200,
            -32602,
        ),
        (
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"merit_scores_list","arguments":{"provider":"gitlab"}}}"#,
            200,
            -32602,
        ),
    ] {
        let (s, v) = post_mcp!(app, t, body);
        assert_eq!(s.as_u16(), status, "{body}: {v}");
        assert_eq!(v["jsonrpc"], "2.0", "{body}: {v}");
        assert_eq!(v["error"]["code"], code, "{body}: {v}");
        assert!(v.get("result").is_none());
    }

    // Unsupported protocol version header: 400.
    let req = atest::TestRequest::post()
        .uri("/mcp")
        .insert_header(("Authorization", format!("Bearer {tok}")))
        .insert_header(("MCP-Protocol-Version", "1999-01-01"))
        .set_payload(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
    let (s, v) = rest!(app, req);
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");

    // The well-formed lifecycle.
    let (s, v) = rpc!(
        app,
        &tok,
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}})
    );
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["id"], 1);
    assert_eq!(v["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(v["result"]["serverInfo"]["name"], SERVER_NAME);
    assert!(v["result"]["capabilities"]["tools"].is_object(), "{v}");
    let (_, v) = rpc!(
        app,
        &tok,
        "initialize",
        json!({"protocolVersion": "2024-11-05"})
    );
    assert_eq!(v["result"]["protocolVersion"], "2024-11-05");
    let (_, v) = rpc!(
        app,
        &tok,
        "initialize",
        json!({"protocolVersion": "1999-01-01"})
    );
    assert_eq!(
        v["result"]["protocolVersion"], SUPPORTED_PROTOCOL_VERSIONS[0],
        "unknown version: offer ours"
    );

    let (s, v) = post_mcp!(
        app,
        t,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
    );
    assert_eq!(s, StatusCode::ACCEPTED);
    assert_eq!(v, Value::Null, "no body for a notification");

    let (s, v) = post_mcp!(app, t, r#"{"jsonrpc":"2.0","id":"p-1","method":"ping"}"#);
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v, json!({"jsonrpc": "2.0", "id": "p-1", "result": {}}));

    let (s, _) = rest!(
        app,
        atest::TestRequest::get()
            .uri("/mcp")
            .insert_header(("Authorization", format!("Bearer {tok}")))
    );
    assert_eq!(s, StatusCode::METHOD_NOT_ALLOWED);
}

#[test]
fn execute_flag_defaults_off() {
    assert!(!parse_allow_execute(None));
    assert!(!parse_allow_execute(Some("")));
    assert!(!parse_allow_execute(Some("1")));
    assert!(!parse_allow_execute(Some("yes")));
    assert!(!parse_allow_execute(Some("false")));
    assert!(parse_allow_execute(Some("true")));
    assert!(parse_allow_execute(Some(" TRUE ")));
}

// ------------------------------------------------------------------ payroll

#[actix_web::test]
async fn payroll_tools_act_as_the_token_member_and_refuse_actor_fields() {
    let pool = pool().await;
    let stripe = Arc::new(MockStripe::default());
    let app = app!(ctx(&pool, stripe.clone(), true));
    let staff_p = crate::auth::Principal {
        token_id: "t".into(),
        org_id: "default".into(),
        member_id: "staff".into(),
        scopes: vec![scopes::PAYEES_MANAGE.into()],
        payee_keys: vec![],
    };
    let payee = crate::payments::payees::create(
        &pool,
        &EnvDirectory::default(),
        &staff_p,
        serde_json::from_value(json!({"display_name": "Alice", "kind": "individual", "payee_keys": ["alice"], "stripe_account_id": "acct_alice"})).unwrap(),
        Utc::now(),
    )
    .await
    .unwrap()
    .row
    .id;
    ledger::update_account_status(&pool, "acct_alice", true)
        .await
        .unwrap();
    let staff = token(&pool, "staff", scopes::ALL, &[]).await;
    let bob = token(&pool, "bob", scopes::ALL, &[]).await;
    let alice = token(&pool, "alice", scopes::ALL, &["alice"]).await;

    let order_args = json!({"name": "Salary", "rule": {"kind": "fixed", "payee_id": payee, "amount_cents": 1500},
        "schedule": {"kind": "once", "date": "2026-01-01"}});
    // Actor fields are protocol errors, nothing is written.
    for (name, args) in [
        ("payorder_create", {
            let mut a = order_args.clone();
            a["created_by"] = json!("mallory");
            a
        }),
        (
            "payorder_approve",
            json!({"order_id": "x", "approver_member_id": "mallory"}),
        ),
        (
            "payorder_pause",
            json!({"order_id": "x", "actor": "mallory"}),
        ),
        (
            "invoice_submit",
            json!({"payee_id": payee, "number": "1", "issue_date": "2026-01-01", "due_date": "2026-01-02",
            "lines": [{"description": "work", "quantity": 1, "unit_amount_cents": 100}], "total_cents": 100, "submitted_by": "mallory"}),
        ),
        (
            "invoice_approve",
            json!({"invoice_id": "x", "approver_member_id": "mallory"}),
        ),
        (
            "payout_statement",
            json!({"payee_id": payee, "from": "2026-01-01", "to": "2026-12-31", "member_id": "mallory"}),
        ),
    ] {
        let (s, v) = rpc!(
            app,
            &staff,
            "tools/call",
            json!({"name": name, "arguments": args})
        );
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["error"]["code"], -32602, "{name}: {v}");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("unknown field"),
            "{v}"
        );
    }
    assert_eq!(count(&pool, "pay_orders").await, 0);
    assert_eq!(count(&pool, "invoices").await, 0);

    // The payee cannot create an order that pays them.
    let v = tool!(app, &alice, "payorder_create", order_args.clone());
    assert!(is_error(&v), "{v}");
    assert!(text(&v).contains("payee"), "{v}");

    let v = tool!(app, &staff, "payorder_create", order_args);
    assert!(!is_error(&v), "{v}");
    assert_eq!(structured(&v)["payorder"]["created_by"], "staff");
    let order_id = structured(&v)["payorder"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let v = tool!(
        app,
        &staff,
        "payorder_approve",
        json!({"order_id": order_id})
    );
    assert!(is_error(&v) && text(&v).contains("creator"), "{v}");
    let v = tool!(
        app,
        &alice,
        "payorder_approve",
        json!({"order_id": order_id})
    );
    assert!(is_error(&v) && text(&v).contains("payee"), "{v}");
    let v = tool!(app, &bob, "payorder_approve", json!({"order_id": order_id}));
    assert!(!is_error(&v), "{v}");
    assert_eq!(structured(&v)["result"]["approver_member_id"], "bob");
    assert_eq!(structured(&v)["result"]["order"]["approved"], true);
    let v = tool!(
        app,
        &bob,
        "payorder_preview",
        json!({"order_id": order_id, "periods": 2})
    );
    assert!(!is_error(&v), "{v}");

    let inv = json!({"payee_id": payee, "number": "INV-1", "issue_date": "2026-01-01", "due_date": "2026-01-15",
        "lines": [{"description": "work", "quantity": 2, "unit_amount_cents": 500}], "tax_cents": 50, "total_cents": 1050});
    let v = tool!(app, &alice, "invoice_submit", inv);
    assert!(!is_error(&v), "{v}");
    assert_eq!(structured(&v)["invoice"]["submitted_by"], "alice");
    let inv_id = structured(&v)["invoice"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let v = tool!(
        app,
        &alice,
        "invoice_approve",
        json!({"invoice_id": inv_id})
    );
    assert!(is_error(&v) && text(&v).contains("payee"), "{v}");
    let v = tool!(app, &bob, "invoice_approve", json!({"invoice_id": inv_id}));
    assert!(!is_error(&v), "{v}");
    assert_eq!(structured(&v)["result"]["invoice"]["status"], "scheduled");

    for (name, args) in [
        ("payees_list", json!({})),
        ("payee_get", json!({"payee_id": payee})),
        ("payorders_list", json!({"status": "active"})),
        ("invoices_list", json!({"status": "scheduled"})),
        (
            "payout_statement",
            json!({"payee_id": payee, "from": "2020-01-01", "to": "2030-01-01"}),
        ),
    ] {
        let v = tool!(app, &bob, name, args);
        assert!(!is_error(&v), "{name}: {v}");
    }
    let v = tool!(
        app,
        &bob,
        "payout_statement",
        json!({"payee_id": payee, "from": "2020-01-01", "to": "2030-01-01"})
    );
    assert_eq!(structured(&v)["statement"]["pending_cents"], 1050);
    let v = tool!(app, &bob, "payorder_pause", json!({"order_id": order_id}));
    assert!(!is_error(&v), "{v}");
    assert_eq!(structured(&v)["payorder"]["status"], "paused");
    // No payroll tool moved money.
    assert_eq!(stripe.transfers.lock().unwrap().len(), 0);
}

// ------------------------------------------------------------------ audit

#[actix_web::test]
async fn audit_tools_and_rest_list_and_verify_the_chain() {
    let pool = pool().await;
    let stripe = Arc::new(MockStripe::default());
    let c = ctx(&pool, stripe, false);
    let api = c.api.clone();
    let app = app!(c);
    let reader = token(&pool, "auditor", &[scopes::AUDIT_READ], &[]).await;
    let other = token(&pool, "bob", &[scopes::PAYOUTS_READ], &[]).await;

    let v = tool!(
        app,
        &reader,
        "audit_list",
        json!({"action": "token.*", "limit": 1})
    );
    assert!(!is_error(&v), "{v}");
    let entries = structured(&v)["entries"].as_array().unwrap().clone();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["action"], "token.created");
    assert_eq!(entries[0]["detail"]["member_id"], "auditor");
    assert_eq!(structured(&v)["next_after"], 1);
    let v = tool!(app, &reader, "audit_verify", json!({}));
    assert_eq!(structured(&v)["verify"]["ok"], true, "{v}");
    assert_eq!(structured(&v)["verify"]["checked"], 2);

    // REST, same scope.
    let rest_app = atest::init_service(
        App::new()
            .app_data(web::Data::new(api))
            .configure(crate::audit::api::configure),
    )
    .await;
    let get = |uri: &str, tok: &str| {
        atest::TestRequest::get()
            .uri(uri)
            .insert_header(("Authorization", format!("Bearer {tok}")))
    };
    let (s, body) = rest!(rest_app, get("/api/v1/audit?after=1", &reader));
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["entries"].as_array().unwrap().len(), 1);
    assert_eq!(body["entries"][0]["detail"]["member_id"], "bob");
    let (s, body) = rest!(rest_app, get("/api/v1/audit/verify", &reader));
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["verify"]["ok"], true);
    let (s, _) = rest!(rest_app, get("/api/v1/audit", &other));
    assert_eq!(s, StatusCode::FORBIDDEN, "audit.read is required");
    let (s, _) = rest!(
        rest_app,
        atest::TestRequest::get().uri("/api/v1/audit/verify")
    );
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

// ------------------------------------------------------------------ time (contract 14.6)

#[actix_web::test]
async fn time_tools_act_only_on_the_token_members_own_time() {
    use crate::payments::{payees, shifts};
    let pool = pool().await;
    let admin = crate::auth::Principal::legacy_admin();
    let bob: payees::NewPayee = serde_json::from_value(
        json!({"display_name": "Bob", "kind": "individual", "member_id": "bob"}),
    )
    .unwrap();
    let bob = payees::create(&pool, &EnvDirectory::default(), &admin, bob, Utc::now())
        .await
        .unwrap()
        .row
        .id;
    let now = Utc::now();
    let from = now - chrono::Duration::hours(1);
    let body = json!({"payee_id": bob, "kind": "single", "date": from.format("%Y-%m-%d").to_string(),
        "start_time": from.format("%H:%M").to_string(), "end_time": (now + chrono::Duration::hours(3)).format("%H:%M").to_string(), "time_zone": "UTC"});
    shifts::create(
        &pool,
        &EnvDirectory::default(),
        &admin,
        serde_json::from_value(body).unwrap(),
        now,
    )
    .await
    .unwrap();
    let app = app!(ctx(&pool, Arc::new(MockStripe::default()), false));
    let alice = token(&pool, "alice", &[scopes::TIME_CLOCK], &[]).await;
    let bob_tok = token(
        &pool,
        "bob",
        &[scopes::TIME_CLOCK, scopes::TIME_APPROVE],
        &[],
    )
    .await;

    // No argument can name a payee or an approver.
    for (tok, name, args) in [
        (&alice, "time_clock_in", json!({"payee_id": bob})),
        (&alice, "time_clock_in", json!({"member_id": "bob"})),
        (
            &bob_tok,
            "time_entry_approve",
            json!({"entry_id": "x", "approver_member_id": "mallory"}),
        ),
    ] {
        let (s, v) = rpc!(
            app,
            tok,
            "tools/call",
            json!({"name": name, "arguments": args})
        );
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["error"]["code"], -32602, "{name}: {v}");
    }
    // alice is not a payee: refused, and bob's shift is never used for her
    let v = tool!(app, &alice, "time_clock_in", json!({}));
    assert!(is_error(&v) && text(&v).contains("not a payee"), "{v}");
    assert_eq!(count(&pool, "time_entries").await, 0);
    // bob clocks in as bob, twice: one entry
    let v = tool!(app, &bob_tok, "time_clock_in", json!({}));
    assert!(!is_error(&v), "{v}");
    assert_eq!(structured(&v)["entry"]["payee_id"], bob.as_str());
    let v = tool!(app, &bob_tok, "time_clock_in", json!({}));
    assert_eq!(structured(&v)["result"], "existing");
    assert_eq!(count(&pool, "time_entries").await, 1);
    // bob can never approve his own time
    let id: String = sqlx::query_scalar("SELECT id FROM time_entries")
        .fetch_one(&pool)
        .await
        .unwrap();
    let v = tool!(app, &bob_tok, "time_entry_approve", json!({"entry_id": id}));
    assert!(
        is_error(&v) && text(&v).contains("cannot approve their own"),
        "{v}"
    );
}
