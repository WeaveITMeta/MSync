//! Portal sync API: the five authentication checks and their single 401, scope and field
//! allowlists, pairwise subjects, the uniform 404, suspension, the rate limit, the audit of
//! every lookup, and the worked example in `docs/PORTAL_API.md`.
//!
//! Fixture: the Voltec Industries example CRA, with a second consumer `voltec-site` added for
//! the pairwise tests. In it: mckale is Engineer (rank 20) with Discord `300000000000000104`;
//! gus-example is Director (70, holds `payouts.approve`) with Discord `300000000000000102`;
//! Voltec has no `[slack]`.

use std::collections::HashMap;
use std::sync::Arc;

use actix_web::http::StatusCode;
use actix_web::{test as atest, web, App};
use chrono::{NaiveDate, Utc};
use serde_json::{json, Value};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};

use super::*;
use crate::cra::loader::{ActiveCra, CraHandle};
use crate::cra::{CraFiles, CraRepo};

const SECRET_K1: &str = "voltec-example-secret-0123456789abcdef";
const SECRET_K2: &str = "voltec-rotated-secret-fedcba9876543210";
const SITE_SECRET: &str = "voltec-site-secret-00112233445566778899";
const SUBJECT_KEY: &str = "msync-example-subject-key-0123456789abcdef";
const MCKALE_DISCORD: &str = "300000000000000104";
const GUS_DISCORD: &str = "300000000000000102";

fn today() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 11, 12).unwrap()
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

fn voltec_files() -> CraFiles {
    let mut org = include_str!("../../examples/cra/voltec-industries/.msync/org.toml").to_string();
    org.push_str(
        "\n[[consumer]]\nkey = \"voltec-site\"\nlabel = \"Voltec site renderer\"\nscopes = [\"community.*\", \"profile.*\"]\nfields = [\"permissions\"]\n",
    );
    CraFiles {
        org,
        roles: include_str!("../../examples/cra/voltec-industries/.msync/roles.toml").into(),
        members: include_str!("../../examples/cra/voltec-industries/.msync/members.toml").into(),
        merit: include_str!("../../examples/cra/voltec-industries/.msync/merit.toml").into(),
        payouts: include_str!("../../examples/cra/voltec-industries/.msync/payouts.toml").into(),
    }
}

fn activate(handle: &CraHandle, files: &CraFiles) {
    let snapshot = crate::cra::load(files, today()).unwrap_or_else(|e| panic!("{e:?}"));
    handle.replace(Some(Arc::new(ActiveCra {
        snapshot_id: 7,
        commit_sha: "c0ffee".into(),
        loaded_at: "2026-11-12T00:00:00Z".into(),
        actor: Some("github:example-founder".into()),
        repo: CraRepo::new(
            crate::foundry::FoundryProvider::GitHub,
            "github.com",
            "example-voltec/cra",
        ),
        snapshot,
    })));
}

fn secrets() -> SecretLookup {
    let m: HashMap<String, String> = [
        ("MSYNC_CONSUMER_SECRET_VOLTEC_PORTAL_K1", SECRET_K1),
        ("MSYNC_CONSUMER_SECRET_VOLTEC_PORTAL_K2", SECRET_K2),
        ("MSYNC_CONSUMER_SECRET_VOLTEC_SITE_K1", SITE_SECRET),
        // Too short: treated as absent.
        ("MSYNC_CONSUMER_SECRET_VOLTEC_PORTAL_SHORT", "short-secret"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    Arc::new(move |name: &str| m.get(name).cloned())
}

async fn ctx_with(files: Option<CraFiles>) -> (Arc<PortalContext>, SqlitePool, CraHandle) {
    let pool = pool().await;
    let handle = CraHandle::new();
    if let Some(f) = files {
        activate(&handle, &f);
    }
    handle.set_merit_for_test("mckale", 60);
    handle.set_merit_for_test("gus-example", 600);
    let mut ctx = PortalContext::new(
        pool.clone(),
        handle.clone(),
        secrets(),
        Some(SUBJECT_KEY.into()),
    );
    ctx.today = Some(today());
    (Arc::new(ctx), pool, handle)
}

macro_rules! app {
    ($ctx:expr) => {
        atest::init_service(
            App::new()
                .app_data(web::Data::from($ctx.clone()))
                .configure(configure),
        )
        .await
    };
}

fn nonce() -> String {
    // 32 hex characters: valid base64url, 128 bits.
    uuid::Uuid::new_v4().simple().to_string()
}

struct Signed<'a> {
    consumer: &'a str,
    key_id: &'a str,
    secret: &'a str,
    ts: i64,
    nonce: String,
}

impl<'a> Signed<'a> {
    fn new(consumer: &'a str, key_id: &'a str, secret: &'a str) -> Self {
        Signed {
            consumer,
            key_id,
            secret,
            ts: Utc::now().timestamp(),
            nonce: nonce(),
        }
    }
}

fn request(path: &str, s: &Signed<'_>, signature: Option<String>) -> atest::TestRequest {
    let ts = s.ts.to_string();
    let sig = signature.unwrap_or_else(|| {
        sign(
            s.secret.as_bytes(),
            &string_to_sign("GET", path, &ts, &s.nonce, b""),
        )
    });
    atest::TestRequest::get()
        .uri(path)
        .insert_header((HEADER_CONSUMER, s.consumer))
        .insert_header((HEADER_KEY_ID, s.key_id))
        .insert_header((HEADER_TIMESTAMP, ts))
        .insert_header((HEADER_NONCE, s.nonce.clone()))
        .insert_header((HEADER_SIGNATURE, sig))
}

macro_rules! call {
    ($app:expr, $req:expr) => {{
        let resp = atest::call_service(&$app, $req.to_request()).await;
        let status = resp.status();
        let bytes = atest::read_body(resp).await;
        (status, bytes.to_vec())
    }};
}

fn body(b: &[u8]) -> Value {
    serde_json::from_slice(b).unwrap_or(Value::Null)
}

fn discord_path(id: &str, scopes: &str) -> String {
    format!("/api/v1/members/by-discord/{id}?scopes={scopes}")
}

async fn count(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

// ------------------------------------------------------------------ authentication

#[actix_web::test]
async fn a_valid_signed_request_is_answered() {
    let (ctx, pool, _) = ctx_with(Some(voltec_files())).await;
    let app = app!(ctx);
    let path = discord_path(MCKALE_DISCORD, "community.*");
    let (s, b) = call!(
        app,
        request(&path, &Signed::new("voltec-portal", "k1", SECRET_K1), None)
    );
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let v = body(&b);
    assert_eq!(v["status"], "active");
    assert_eq!(
        v["subject"],
        json!(subject_for(
            SUBJECT_KEY.as_bytes(),
            "default",
            "voltec-portal",
            "mckale"
        ))
    );
    assert_eq!(
        v["tier"],
        json!({"key": "contributor", "label": "Contributor", "rank": 10})
    );
    assert_eq!(v["positions"], json!([{"title": "Engineer"}]));
    assert_eq!(v["merit"], json!({"vested": 60}));
    assert_eq!(
        v["permissions"],
        json!(["community.post", "community.read", "community.vote"])
    );
    assert_eq!(v["scopes_granted"], json!(["community.*"]));
    assert_eq!(v["policy_version"], json!(7));
    assert_eq!(v["cache_ttl_seconds"], json!(60));

    // A rotated key works alongside the old one.
    let (s, _) = call!(
        app,
        request(&path, &Signed::new("voltec-portal", "k2", SECRET_K2), None)
    );
    assert_eq!(s, StatusCode::OK);

    // Recorded and audited.
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM consumer_lookups WHERE result = 'found'"
        )
        .await,
        2
    );
    let a = crate::audit::list(
        &pool,
        &crate::audit::AuditFilter {
            action: Some("portal.lookup".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(a.entries.len(), 2);
    assert_eq!(a.entries[0].actor, "consumer:voltec-portal");
    assert_eq!(a.entries[0].detail["member_id"], json!("mckale"));
    assert_eq!(
        a.entries[0].detail["scopes_granted"],
        json!(["community.*"])
    );
    assert!(crate::audit::verify(&pool).await.unwrap().ok);
}

#[actix_web::test]
async fn every_authentication_failure_is_the_same_401() {
    let (ctx, pool, _) = ctx_with(Some(voltec_files())).await;
    let app = app!(ctx);
    let path = discord_path(MCKALE_DISCORD, "community.*");

    // Control: the correct request passes.
    let replayed = Signed::new("voltec-portal", "k1", SECRET_K1);
    let (s, _) = call!(app, request(&path, &replayed, None));
    assert_eq!(s, StatusCode::OK);

    let mut cases: Vec<(&str, atest::TestRequest)> = Vec::new();
    let wrong = Signed::new("voltec-portal", "k1", SECRET_K1);
    let bad_sig = sign(
        b"not-the-secret-not-the-secret-00000000",
        &string_to_sign("GET", &path, &wrong.ts.to_string(), &wrong.nonce, b""),
    );
    cases.push(("wrong signature", request(&path, &wrong, Some(bad_sig))));
    let mut stale = Signed::new("voltec-portal", "k1", SECRET_K1);
    stale.ts -= MAX_SKEW_SECS + 1;
    cases.push(("stale timestamp", request(&path, &stale, None)));
    let mut future = Signed::new("voltec-portal", "k1", SECRET_K1);
    future.ts += MAX_SKEW_SECS + 1;
    cases.push(("future timestamp", request(&path, &future, None)));
    cases.push(("reused nonce", request(&path, &replayed, None)));
    cases.push((
        "unknown consumer",
        request(&path, &Signed::new("acme-portal", "k1", SECRET_K1), None),
    ));
    cases.push((
        "unknown key id",
        request(&path, &Signed::new("voltec-portal", "k9", SECRET_K1), None),
    ));
    cases.push((
        "short secret",
        request(
            &path,
            &Signed::new("voltec-portal", "short", "short-secret"),
            None,
        ),
    ));
    // The path is part of the signature: a signature for mckale does not open gus.
    let other = Signed::new("voltec-portal", "k1", SECRET_K1);
    let sig_for_mckale = sign(
        SECRET_K1.as_bytes(),
        &string_to_sign("GET", &path, &other.ts.to_string(), &other.nonce, b""),
    );
    cases.push((
        "signature for another path",
        request(
            &discord_path(GUS_DISCORD, "community.*"),
            &other,
            Some(sig_for_mckale),
        ),
    ));
    cases.push(("missing headers", atest::TestRequest::get().uri(&path)));

    let mut bodies = Vec::new();
    for (name, req) in cases {
        let (s, b) = call!(app, req);
        assert_eq!(
            s,
            StatusCode::UNAUTHORIZED,
            "{name}: {}",
            String::from_utf8_lossy(&b)
        );
        bodies.push((name, b));
    }
    for (name, b) in &bodies {
        assert_eq!(
            b,
            &br#"{"error":"unauthorized"}"#.to_vec(),
            "{name}: body differs"
        );
    }
    // Refused requests answered nothing and recorded no lookup.
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM consumer_lookups").await,
        1
    );
}

#[actix_web::test]
async fn the_rate_limit_trips_per_consumer() {
    let (ctx, _, _) = ctx_with(Some(voltec_files())).await;
    let mut c = Arc::try_unwrap(ctx).ok().expect("one owner");
    c.rate_per_minute = 3;
    let ctx = Arc::new(c);
    let app = app!(ctx);
    let path = discord_path(MCKALE_DISCORD, "community.*");
    for i in 0..3 {
        let (s, _) = call!(
            app,
            request(&path, &Signed::new("voltec-portal", "k1", SECRET_K1), None)
        );
        assert_eq!(s, StatusCode::OK, "request {i}");
    }
    let (s, b) = call!(
        app,
        request(&path, &Signed::new("voltec-portal", "k1", SECRET_K1), None)
    );
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(b, br#"{"error":"unauthorized"}"#.to_vec());
    // Another consumer has its own budget.
    let (s, _) = call!(
        app,
        request(&path, &Signed::new("voltec-site", "k1", SITE_SECRET), None)
    );
    assert_eq!(s, StatusCode::OK);
}

#[actix_web::test]
async fn expired_nonces_are_swept() {
    let (ctx, pool, _) = ctx_with(Some(voltec_files())).await;
    let app = app!(ctx);
    let old = Utc::now().timestamp() - NONCE_TTL_SECS - 5;
    sqlx::query("INSERT INTO consumer_nonces (consumer, nonce, seen_at) VALUES ('voltec-portal', 'old-nonce-old-nonce-old-nonce', ?)")
        .bind(old)
        .execute(&pool)
        .await
        .unwrap();
    let (s, _) = call!(
        app,
        request(
            &discord_path(MCKALE_DISCORD, "community.*"),
            &Signed::new("voltec-portal", "k1", SECRET_K1),
            None
        )
    );
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM consumer_nonces WHERE nonce LIKE 'old-nonce%'"
        )
        .await,
        0
    );
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM consumer_nonces").await,
        1
    );
}

// ------------------------------------------------------------------ what a consumer sees

#[actix_web::test]
async fn a_scope_outside_the_allowlist_returns_nothing_more() {
    let (ctx, pool, _) = ctx_with(Some(voltec_files())).await;
    let app = app!(ctx);
    let signed = |p: &str| request(p, &Signed::new("voltec-portal", "k1", SECRET_K1), None);

    let (_, base) = call!(app, signed(&discord_path(MCKALE_DISCORD, "community.*")));
    let (_, wider) = call!(
        app,
        signed(&discord_path(
            MCKALE_DISCORD,
            "community.*,roster.*,payouts.*,members.grant"
        ))
    );
    let (base, wider) = (body(&base), body(&wider));
    assert_eq!(wider["permissions"], base["permissions"]);
    assert_eq!(wider["scopes_granted"], json!(["community.*"]));

    let (s, outside) = call!(
        app,
        signed(&discord_path(MCKALE_DISCORD, "roster.*,payouts.read_own"))
    );
    assert_eq!(s, StatusCode::OK);
    let outside = body(&outside);
    assert_eq!(
        outside["permissions"],
        json!([]),
        "mckale holds roster.read_members and payouts.read_own; none may leave"
    );
    assert_eq!(outside["scopes_granted"], json!([]));

    // A narrower request gets the narrower set.
    let (_, narrow) = call!(app, signed(&discord_path(MCKALE_DISCORD, "community.post")));
    assert_eq!(body(&narrow)["permissions"], json!(["community.post"]));

    // The refused scopes are on the audit record.
    let a = crate::audit::list(
        &pool,
        &crate::audit::AuditFilter {
            action: Some("portal.lookup".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        a.entries[1].detail["scopes_refused"],
        json!(["roster.*", "payouts.*", "members.grant"])
    );
}

#[actix_web::test]
async fn voltec_never_receives_a_permission_outside_community() {
    let (ctx, _, handle) = ctx_with(Some(voltec_files())).await;
    // Control: gus really holds authority in MSync.
    let active = handle.active().unwrap();
    let st = crate::cra::standing(&active.snapshot, "gus-example", 600, today()).unwrap();
    assert!(st.permissions.contains("payouts.approve"));
    assert!(st.permissions.contains("members.grant"));
    assert!(st.permissions.contains("community.events"));

    let app = app!(ctx);
    for scopes in [
        "community.*",
        "community.*,payouts.*,members.*,audit.*,roster.*,policy.*,consumers.*",
        "payouts.approve",
    ] {
        let (s, b) = call!(
            app,
            request(
                &discord_path(GUS_DISCORD, scopes),
                &Signed::new("voltec-portal", "k1", SECRET_K1),
                None
            )
        );
        assert_eq!(s, StatusCode::OK);
        let v = body(&b);
        let perms: Vec<String> = serde_json::from_value(v["permissions"].clone()).unwrap();
        for p in &perms {
            assert!(p.starts_with("community."), "{scopes}: Voltec received {p}");
        }
        assert!(!perms.contains(&"payouts.approve".to_string()));
        if scopes.starts_with("community") {
            assert!(perms.contains(&"community.events".to_string()), "{perms:?}");
        }
    }
}

#[actix_web::test]
async fn subjects_are_pairwise_and_stable() {
    let (ctx, _, _) = ctx_with(Some(voltec_files())).await;
    let app = app!(ctx);
    let path = discord_path(MCKALE_DISCORD, "community.*");
    let (_, a1) = call!(
        app,
        request(&path, &Signed::new("voltec-portal", "k1", SECRET_K1), None)
    );
    let (_, a2) = call!(
        app,
        request(&path, &Signed::new("voltec-portal", "k2", SECRET_K2), None)
    );
    let (_, b1) = call!(
        app,
        request(&path, &Signed::new("voltec-site", "k1", SITE_SECRET), None)
    );
    let (a1, a2, b1) = (
        body(&a1)["subject"].clone(),
        body(&a2)["subject"].clone(),
        body(&b1)["subject"].clone(),
    );
    assert_eq!(a1, a2, "the same consumer always gets the same subject");
    assert_ne!(
        a1, b1,
        "two consumers get different subjects for the same member"
    );
    let a1 = a1.as_str().unwrap().to_string();
    assert!(a1.starts_with("sub_") && a1.len() == 36, "{a1}");
    assert!(!a1.contains("mckale"));

    // The subject endpoint answers its own consumer with the same member.
    let p = format!("/api/v1/members/{a1}?scopes=community.*");
    let (s, b) = call!(
        app,
        request(&p, &Signed::new("voltec-portal", "k1", SECRET_K1), None)
    );
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body(&b)["subject"], json!(a1));
}

#[actix_web::test]
async fn unknown_and_invisible_members_get_the_identical_404() {
    let (ctx, _, _) = ctx_with(Some(voltec_files())).await;
    let app = app!(ctx);
    let site_subject = subject_for(SUBJECT_KEY.as_bytes(), "default", "voltec-site", "mckale");
    let paths = [
        (
            "unknown discord id",
            discord_path("399999999999999999", "community.*"),
        ),
        (
            "slack id in an org without [slack]",
            "/api/v1/members/by-slack/T1000000000/U1000000102?scopes=community.*".to_string(),
        ),
        (
            "another consumer's subject",
            format!("/api/v1/members/{site_subject}?scopes=community.*"),
        ),
        (
            "unknown subject",
            format!("/api/v1/members/sub_{}?scopes=community.*", "0".repeat(32)),
        ),
        (
            "not a subject",
            "/api/v1/members/mckale?scopes=community.*".to_string(),
        ),
    ];
    for (name, p) in paths {
        let (s, b) = call!(
            app,
            request(&p, &Signed::new("voltec-portal", "k1", SECRET_K1), None)
        );
        assert_eq!(s, StatusCode::NOT_FOUND, "{name}");
        assert_eq!(b, br#"{"error":"not_found"}"#.to_vec(), "{name}");
    }

    // With no CRA snapshot active, every lookup is the same 404.
    let (none, _, _) = ctx_with(None).await;
    let app = app!(none);
    let (s, b) = call!(
        app,
        request(
            &discord_path(MCKALE_DISCORD, "community.*"),
            &Signed::new("voltec-portal", "k1", SECRET_K1),
            None
        )
    );
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(b, br#"{"error":"not_found"}"#.to_vec());
}

#[actix_web::test]
async fn a_slack_member_is_found_only_in_the_cra_team() {
    // Eustress Engine runs Slack beside Discord; its consumer is eustress-site.
    let pool = pool().await;
    let handle = CraHandle::new();
    let files = CraFiles {
        org: include_str!("../../examples/cra/eustress-engine/.msync/org.toml").into(),
        roles: include_str!("../../examples/cra/eustress-engine/.msync/roles.toml").into(),
        members: include_str!("../../examples/cra/eustress-engine/.msync/members.toml").into(),
        merit: include_str!("../../examples/cra/eustress-engine/.msync/merit.toml").into(),
        payouts: include_str!("../../examples/cra/eustress-engine/.msync/payouts.toml").into(),
    };
    activate(&handle, &files);
    let secrets: SecretLookup = Arc::new(|n: &str| {
        (n == "MSYNC_CONSUMER_SECRET_EUSTRESS_SITE_K1").then(|| SECRET_K1.to_string())
    });
    let mut c = PortalContext::new(pool, handle, secrets, Some(SUBJECT_KEY.into()));
    c.today = Some(today());
    let ctx = Arc::new(c);
    let app = app!(ctx);
    let (s, b) = call!(
        app,
        request(
            "/api/v1/members/by-slack/T1000000000/U1000000102?scopes=community.*",
            &Signed::new("eustress-site", "k1", SECRET_K1),
            None
        )
    );
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    assert_eq!(body(&b)["positions"], json!([{"title": "Lead Maintainer"}]));
    let (s, b) = call!(
        app,
        request(
            "/api/v1/members/by-slack/T9999999999/U1000000102?scopes=community.*",
            &Signed::new("eustress-site", "k1", SECRET_K1),
            None
        )
    );
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(b, br#"{"error":"not_found"}"#.to_vec());
}

#[actix_web::test]
async fn a_suspended_member_gets_status_and_an_empty_permission_list() {
    let mut files = voltec_files();
    let at = files.members.find("id = \"ivo-example\"").unwrap();
    let tail = files.members[at..].replacen("status = \"active\"", "status = \"suspended\"", 1);
    files.members = format!("{}{}", &files.members[..at], tail);
    let (ctx, _, _) = ctx_with(Some(files)).await;
    let app = app!(ctx);
    let (s, b) = call!(
        app,
        request(
            &discord_path("300000000000000105", "community.*"),
            &Signed::new("voltec-portal", "k1", SECRET_K1),
            None
        )
    );
    assert_eq!(s, StatusCode::OK);
    let v = body(&b);
    assert_eq!(v["status"], "suspended");
    assert_eq!(v["permissions"], json!([]));
    assert!(
        v.get("tier").is_none() && v.get("positions").is_none() && v.get("merit").is_none(),
        "{v}"
    );
    // Even the one permission a suspension leaves in MSync (profile.self) is not handed out:
    // voltec-site may read profile.*.
    let (s, b) = call!(
        app,
        request(
            &discord_path("300000000000000105", "community.*,profile.*"),
            &Signed::new("voltec-site", "k1", SITE_SECRET),
            None
        )
    );
    assert_eq!(s, StatusCode::OK);
    let v = body(&b);
    assert_eq!(v["scopes_granted"], json!(["community.*", "profile.*"]));
    assert_eq!(v["status"], "suspended");
    assert_eq!(v["permissions"], json!([]));
}

#[actix_web::test]
async fn a_field_outside_the_allowlist_is_omitted_not_null() {
    let (ctx, _, _) = ctx_with(Some(voltec_files())).await;
    let app = app!(ctx);
    // voltec-portal: tier, positions.title, merit.vested, permissions.
    let (_, b) = call!(
        app,
        request(
            &discord_path(GUS_DISCORD, "community.*"),
            &Signed::new("voltec-portal", "k1", SECRET_K1),
            None
        )
    );
    let v = body(&b);
    let pos = v["positions"].as_array().unwrap();
    assert_eq!(pos.len(), 1);
    assert_eq!(
        pos[0].as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["title"],
        "rank and expires_at are not allowed"
    );
    // voltec-site: permissions only.
    let (_, b) = call!(
        app,
        request(
            &discord_path(GUS_DISCORD, "community.*"),
            &Signed::new("voltec-site", "k1", SITE_SECRET),
            None
        )
    );
    let v = body(&b);
    let mut keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "cache_ttl_seconds",
            "issued_at",
            "permissions",
            "policy_version",
            "scopes_granted",
            "status",
            "subject"
        ]
    );
}

// ------------------------------------------------------------------ recording

#[actix_web::test]
async fn a_lookup_that_cannot_be_audited_is_not_answered() {
    let (ctx, pool, _) = ctx_with(Some(voltec_files())).await;
    let app = app!(ctx);
    sqlx::query("CREATE TRIGGER audit_refuse BEFORE INSERT ON audit_log BEGIN SELECT RAISE(ABORT, 'down'); END")
        .execute(&pool)
        .await
        .unwrap();
    let (s, b) = call!(
        app,
        request(
            &discord_path(MCKALE_DISCORD, "community.*"),
            &Signed::new("voltec-portal", "k1", SECRET_K1),
            None
        )
    );
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!String::from_utf8_lossy(&b).contains("community"));
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM consumer_lookups").await,
        0,
        "recorded and audited together, or neither"
    );
}

#[actix_web::test]
async fn a_lookup_rate_above_the_threshold_raises_one_alert() {
    let (ctx, pool, _) = ctx_with(Some(voltec_files())).await;
    let mut c = Arc::try_unwrap(ctx).ok().expect("one owner");
    c.alert_lookups_per_hour = 2;
    let ctx = Arc::new(c);
    let app = app!(ctx);
    for id in [
        "399999999999999991",
        "399999999999999992",
        "399999999999999993",
        "399999999999999994",
    ] {
        let _ = call!(
            app,
            request(
                &discord_path(id, "community.*"),
                &Signed::new("voltec-portal", "k1", SECRET_K1),
                None
            )
        );
    }
    let a = crate::audit::list(
        &pool,
        &crate::audit::AuditFilter {
            action: Some("portal.lookup_alert".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(a.entries.len(), 1, "once an hour, not once a request");
    assert_eq!(a.entries[0].detail["consumer"], json!("voltec-portal"));
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM consumer_lookups WHERE result = 'not_found'"
        )
        .await,
        4
    );
}

#[actix_web::test]
async fn no_subject_key_means_no_answer() {
    let pool = pool().await;
    let handle = CraHandle::new();
    activate(&handle, &voltec_files());
    let ctx = Arc::new(PortalContext::new(
        pool,
        handle,
        secrets(),
        Some("too-short".into()),
    ));
    assert!(!ctx.subject_key_configured());
    let app = app!(ctx);
    let (s, _) = call!(
        app,
        request(
            &discord_path(MCKALE_DISCORD, "community.*"),
            &Signed::new("voltec-portal", "k1", SECRET_K1),
            None
        )
    );
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
}

// ------------------------------------------------------------------ the documented example

const DOC: &str = include_str!("../../docs/PORTAL_API.md");
const DOC_SIGNATURE: &str = "v1=9b28b2b50864702de01fb3cc4558942636062e177f64ea5407b23f045600a52a";

#[test]
fn the_worked_example_in_the_doc_matches() {
    let sts = string_to_sign(
        "GET",
        "/api/v1/members/by-discord/300000000000000104?scopes=community.*",
        "1794470400",
        "AAECAwQFBgcICQoLDA0ODw",
        b"",
    );
    assert_eq!(
        sts,
        "v1\nGET\n/api/v1/members/by-discord/300000000000000104?scopes=community.*\n1794470400\nAAECAwQFBgcICQoLDA0ODw\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(sign(SECRET_K1.as_bytes(), &sts), DOC_SIGNATURE);
    assert!(
        DOC.contains(DOC_SIGNATURE),
        "docs/PORTAL_API.md must show the signature the code computes"
    );
    assert!(
        DOC.contains(SECRET_K1)
            && DOC.contains("AAECAwQFBgcICQoLDA0ODw")
            && DOC.contains("1794470400")
    );
    assert_eq!(
        secret_env_name("voltec-portal", "k1"),
        "MSYNC_CONSUMER_SECRET_VOLTEC_PORTAL_K1"
    );
    assert!(DOC.contains("MSYNC_CONSUMER_SECRET_VOLTEC_PORTAL_K1"));
    // The subjects the walkthrough shows.
    for (consumer, expected) in [
        ("voltec-portal", "sub_508b135828168375d957ebd223172547"),
        ("eustress-site", "sub_940b7c756e9c611a646c72f653dce73d"),
    ] {
        assert_eq!(
            subject_for(SUBJECT_KEY.as_bytes(), "default", consumer, "mckale"),
            expected
        );
        assert!(DOC.contains(expected), "{expected} missing from the doc");
    }
}

#[test]
fn scope_intersection_rules() {
    let allow = vec!["community.*".to_string(), "profile.self".to_string()];
    let eff =
        |r: &[&str]| effective_scopes(&r.iter().map(|s| s.to_string()).collect::<Vec<_>>(), &allow);
    assert_eq!(eff(&["community.*"]), vec!["community.*"]);
    assert_eq!(eff(&["community.post"]), vec!["community.post"]);
    assert_eq!(eff(&["profile.*"]), vec!["profile.self"]);
    assert_eq!(eff(&["payouts.*", "*"]), Vec::<String>::new());
    assert_eq!(eff(&[]), Vec::<String>::new());
    assert!(permission_in_scopes(
        "community.post",
        &["community.*".into()]
    ));
    assert!(!permission_in_scopes(
        "communityx.post",
        &["community.*".into()]
    ));
    assert!(!permission_in_scopes(
        "payouts.approve",
        &["community.*".into()]
    ));
}

fn eustress_files() -> CraFiles {
    CraFiles {
        org: include_str!("../../examples/cra/eustress-engine/.msync/org.toml").into(),
        roles: include_str!("../../examples/cra/eustress-engine/.msync/roles.toml").into(),
        members: include_str!("../../examples/cra/eustress-engine/.msync/members.toml").into(),
        merit: include_str!("../../examples/cra/eustress-engine/.msync/merit.toml").into(),
        payouts: include_str!("../../examples/cra/eustress-engine/.msync/payouts.toml").into(),
    }
}

fn weave_files() -> CraFiles {
    CraFiles {
        org: include_str!("../../examples/cra/weave-solutions/.msync/org.toml").into(),
        roles: include_str!("../../examples/cra/weave-solutions/.msync/roles.toml").into(),
        members: include_str!("../../examples/cra/weave-solutions/.msync/members.toml").into(),
        merit: include_str!("../../examples/cra/weave-solutions/.msync/merit.toml").into(),
        payouts: include_str!("../../examples/cra/weave-solutions/.msync/payouts.toml").into(),
    }
}

/// The bodies `docs/PORTAL_API.md` shows for McKale under the three example CRAs.
#[test]
fn the_walkthrough_in_the_doc_matches() {
    let issued = "2026-11-21T19:00:00Z";
    let view = |files: &CraFiles, consumer: &str, requested: &[&str]| {
        let snapshot = crate::cra::load(files, today()).unwrap();
        let active = ActiveCra {
            snapshot_id: 1,
            commit_sha: "c0ffee".into(),
            loaded_at: issued.into(),
            actor: None,
            repo: CraRepo::new(
                crate::foundry::FoundryProvider::GitHub,
                "github.com",
                "example/cra",
            ),
            snapshot,
        };
        let c = active
            .snapshot
            .org
            .consumers
            .iter()
            .find(|c| c.key == consumer)
            .unwrap()
            .clone();
        let member = active.snapshot.member("mckale").unwrap().clone();
        let granted = effective_scopes(
            &requested.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &c.scopes,
        );
        let subject = subject_for(SUBJECT_KEY.as_bytes(), "default", consumer, "mckale");
        member_view(
            &active,
            &c,
            &member,
            &subject,
            0,
            &granted,
            NaiveDate::from_ymd_opt(2026, 11, 21).unwrap(),
            issued,
        )
    };
    let voltec = view(&voltec_files(), "voltec-portal", &["community.*"]);
    assert_eq!(
        voltec,
        json!({
            "subject": "sub_508b135828168375d957ebd223172547",
            "status": "active",
            "tier": { "key": "newcomer", "label": "Newcomer", "rank": 0 },
            "positions": [ { "title": "Engineer" } ],
            "merit": { "vested": 0 },
            "permissions": ["community.post", "community.read", "community.vote"],
            "scopes_granted": ["community.*"],
            "policy_version": 1,
            "issued_at": "2026-11-21T19:00:00Z",
            "cache_ttl_seconds": 60
        })
    );
    let eustress = view(
        &eustress_files(),
        "eustress-site",
        &["community.*", "profile.*"],
    );
    assert_eq!(
        eustress,
        json!({
            "subject": "sub_940b7c756e9c611a646c72f653dce73d",
            "status": "active",
            "tier": { "key": "newcomer", "label": "Newcomer", "rank": 0 },
            "positions": [ { "title": "Lead Maintainer" } ],
            "merit": { "vested": 0 },
            "permissions": ["community.read", "profile.self"],
            "scopes_granted": ["community.*", "profile.*"],
            "policy_version": 1,
            "issued_at": "2026-11-21T19:00:00Z",
            "cache_ttl_seconds": 60
        })
    );
    for line in [
        r#""permissions": ["community.post", "community.read", "community.vote"],"#,
        r#""permissions": ["community.read", "profile.self"],"#,
        r#""scopes_granted": ["community.*", "profile.*"],"#,
        r#""positions": [ { "title": "Lead Maintainer" } ],"#,
    ] {
        assert!(DOC.contains(line), "doc is missing {line}");
    }
    // Weave declares no consumer: nobody can authenticate.
    let weave = crate::cra::load(&weave_files(), today()).unwrap();
    assert!(weave.org.consumers.is_empty());
    assert!(weave.member("mckale").is_some());
}

#[actix_web::test]
async fn weave_answers_every_request_with_401() {
    let (ctx, _, _) = ctx_with(Some(weave_files())).await;
    let app = app!(ctx);
    let (s, b) = call!(
        app,
        request(
            "/api/v1/members/by-slack/T2000000000/U2000000102?scopes=community.*",
            &Signed::new("voltec-portal", "k1", SECRET_K1),
            None
        )
    );
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(b, br#"{"error":"unauthorized"}"#.to_vec());
}
