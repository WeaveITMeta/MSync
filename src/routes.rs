use crate::cra::loader::{CraContext, CraHandle, CraRuntimeError, LoadOutcome};
use crate::foundry::{self, FoundryProcessor, ProcessOutcome, RepoDecision, RepoRegistry};
use crate::github::webhook::{GitHubWebhookHandler, GitHubWebhookPayload};
use crate::merit::leaderboard::{self, LeaderboardQuery};
use crate::merit::scoring::MeritRules;
use crate::payments::PaymentConfig;
use crate::AppState;
use actix_web::http::header::HeaderMap;
use actix_web::http::StatusCode;
use actix_web::{get, post, web, HttpRequest, HttpResponse, Responder};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::sync::Arc;
use tracing::{error, info, warn};

/// Merit rules in force: the active CRA snapshot's `merit.toml`, or the contract 3.4
/// defaults when no snapshot is active.
pub fn merit_rules(state: &AppState) -> Arc<MeritRules> {
    merit_rules_for(&state.cra.handle)
}

pub fn merit_rules_for(handle: &CraHandle) -> Arc<MeritRules> {
    match handle.active() {
        Some(a) => Arc::new(crate::cra::loader::merit_rules_from_cra(&a.snapshot.merit)),
        None => Arc::new(MeritRules::default()),
    }
}

/// Repository registry in force: the active CRA snapshot's `org.toml` `[[foundry]]`, or
/// `MSYNC_FOUNDRY_REPOS` when no snapshot is active.
pub fn foundry_registry(state: &AppState) -> Arc<RepoRegistry> {
    foundry_registry_for(&state.cra.handle, &state.config.foundry_repos)
}

pub fn foundry_registry_for(handle: &CraHandle, fallback: &RepoRegistry) -> Arc<RepoRegistry> {
    match handle.active() {
        Some(a) => Arc::new(crate::cra::loader::registry_from_cra(&a.snapshot.org)),
        None => Arc::new(fallback.clone()),
    }
}

/// The JSON a webhook response carries for a CRA load.
fn cra_json(r: &Result<LoadOutcome, CraRuntimeError>) -> serde_json::Value {
    match r {
        Ok(o) => serde_json::to_value(o).unwrap_or(serde_json::Value::Null),
        Err(e) => serde_json::json!({"outcome": "error", "message": e.to_string()}),
    }
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub uptime_seconds: u64,
    /// `true` when `MSYNC_DEV_MODE=true`: this instance is not safe on a reachable host.
    pub dev_mode: bool,
}

#[derive(Debug, Deserialize)]
pub struct StatsQuery {
    pub github_username: String,
}

/// Process start, for `uptime_seconds`.
static STARTED: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Record the process start (called once at startup; later calls do nothing).
pub fn mark_started() {
    let _ = STARTED.set(std::time::Instant::now());
}

#[get("/health")]
pub async fn health_check(req: HttpRequest) -> impl Responder {
    let dev_mode = req.app_data::<web::Data<AppState>>().is_some_and(|s| s.config.dev_mode);
    HttpResponse::Ok().json(HealthResponse {
        status: "healthy".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_seconds: STARTED.get().map_or(0, |t| t.elapsed().as_secs()),
        dev_mode,
    })
}

/// Verify a GitHub `X-Hub-Signature-256` header ("sha256=<hex>") against the raw body
/// using HMAC-SHA256 with the shared webhook secret. Comparison is constant-time.
pub fn verify_github_signature(secret: &[u8], body: &[u8], header: Option<&str>) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use subtle::ConstantTimeEq;

    if secret.is_empty() {
        return false;
    }
    let Some(hex_sig) = header.and_then(|h| h.strip_prefix("sha256=")) else {
        return false;
    };
    let Ok(provided) = hex::decode(hex_sig.trim()) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    let expected = mac.finalize().into_bytes();
    expected.as_slice().ct_eq(provided.as_slice()).into()
}

#[post("/github/webhook")]
pub async fn github_webhook(
    req: HttpRequest,
    body: web::Bytes,
    state: web::Data<AppState>,
) -> impl Responder {
    // Verify webhook signature over the raw request body before parsing anything
    let signature = req
        .headers()
        .get("X-Hub-Signature-256")
        .and_then(|h| h.to_str().ok());

    if !verify_github_signature(
        state.config.github.webhook_secret.as_bytes(),
        &body,
        signature,
    ) {
        error!("Rejected GitHub webhook: missing or invalid X-Hub-Signature-256");
        return HttpResponse::Unauthorized().json(serde_json::json!({
            "status": "error",
            "message": "invalid signature"
        }));
    }

    let payload: GitHubWebhookPayload = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "status": "error",
                "message": format!("invalid payload: {}", e)
            }));
        }
    };

    let event_type = req
        .headers()
        .get("X-GitHub-Event")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("unknown");

    info!("Received GitHub webhook event: {}", event_type);

    // Drop exact redeliveries (GitHub reuses X-GitHub-Delivery on redelivery)
    let delivery_key = req
        .headers()
        .get("X-GitHub-Delivery")
        .and_then(|h| h.to_str().ok())
        .map(|d| format!("delivery:{}", d));
    if let Some(key) = &delivery_key {
        match crate::github::dedupe::claim(&state.db_pool, key).await {
            Ok(true) => {}
            Ok(false) => {
                info!("Duplicate GitHub delivery {}; ignoring", key);
                return HttpResponse::Ok().json(serde_json::json!({"status": "duplicate", "event": event_type}));
            }
            Err(e) => {
                error!("dedupe claim failed: {}", e);
                return HttpResponse::InternalServerError().json(serde_json::json!({"status": "error"}));
            }
        }
    }

    let handler = GitHubWebhookHandler::new(state.db_pool.clone())
        .with_payments(state.payments.clone())
        .with_registry(foundry_registry(&state))
        .with_rules(merit_rules(&state));

    match handler.handle_webhook(event_type, payload).await {
        Ok(_) => {
            // CRA loading: only a push to the CRA repository's configured branch loads anything.
            let cra = github_cra_push(&state.cra, event_type, &body, delivery_key.as_deref()).await;
            match cra {
                None => {
                    state.cra.refresh_merits().await;
                    HttpResponse::Ok().json(serde_json::json!({
                        "status": "success",
                        "event": event_type
                    }))
                }
                Some(Err(e)) => {
                    error!("CRA load failed: {}", e);
                    if let Some(key) = &delivery_key {
                        let _ = crate::github::dedupe::release(&state.db_pool, key).await;
                    }
                    HttpResponse::InternalServerError().json(serde_json::json!({
                        "status": "error",
                        "message": "CRA load failed",
                    }))
                }
                Some(r) => {
                    // A rejection caused by the Foundry API may be retried: let a redelivery through.
                    if matches!(r, Ok(LoadOutcome::Rejected { retryable: true, .. })) {
                        if let Some(key) = &delivery_key {
                            let _ = crate::github::dedupe::release(&state.db_pool, key).await;
                        }
                    }
                    HttpResponse::Ok().json(serde_json::json!({
                        "status": "success",
                        "event": event_type,
                        "cra": cra_json(&r),
                    }))
                }
            }
        }
        Err(e) => {
            error!("Failed to handle webhook: {}", e);
            if let Some(key) = &delivery_key {
                let _ = crate::github::dedupe::release(&state.db_pool, key).await;
            }
            HttpResponse::InternalServerError().json(serde_json::json!({
                "status": "error",
                "message": e.to_string()
            }))
        }
    }
}

/// The CRA side of a verified GitHub webhook. `None` unless this is a branch push to
/// the CRA repository's configured branch. The body is used only for the branch, the
/// head SHA and the authenticated sender; the files are fetched from the API at the SHA.
pub async fn github_cra_push(
    cra: &CraContext,
    event_type: &str,
    body: &[u8],
    delivery: Option<&str>,
) -> Option<Result<LoadOutcome, CraRuntimeError>> {
    if event_type != "push" {
        return None;
    }
    let push: foundry::github::GitHubPushPayload = match serde_json::from_slice(body) {
        Ok(p) => p,
        Err(e) => {
            warn!("GitHub push payload did not parse: {}", e);
            return None;
        }
    };
    let ev = foundry::github::normalize_push(&push, delivery)?;
    cra.on_push(&ev, chrono::Utc::now().date_naive()).await
}

#[get("/api/contributor/stats")]
pub async fn get_contributor_stats(
    query: web::Query<StatsQuery>,
    state: web::Data<AppState>,
) -> impl Responder {
    // Runtime queries (no compile-time database): a fresh clone builds with no DATABASE_URL.
    #[derive(sqlx::FromRow)]
    struct ContributorRow {
        id: String,
        github_username: String,
        total_merit_points: i64,
        current_role: String,
        contributions_count: i64,
        created_at: String,
    }
    let contributor = sqlx::query_as::<_, ContributorRow>(
        "SELECT id, github_username, total_merit_points, current_role, contributions_count, created_at          FROM contributors WHERE github_username = ?",
    )
    .bind(&query.github_username)
    .fetch_optional(&state.db_pool)
    .await;

    match contributor {
        Ok(Some(contrib)) => {
            // Recent activities
            let activities: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM (SELECT 1 FROM merit_activities WHERE contributor_id = ?                  ORDER BY created_at DESC LIMIT 10)",
            )
            .bind(&contrib.id)
            .fetch_one(&state.db_pool)
            .await
            .unwrap_or(0);

            // Calculate statistics
            let merged_count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM contributions WHERE contributor_id = ? AND status = 'Merged'",
            )
            .bind(&contrib.id)
            .fetch_one(&state.db_pool)
            .await
            .unwrap_or(0);

            let avg_score: f64 = sqlx::query_scalar::<_, Option<f64>>(
                "SELECT AVG(coderabbit_score) FROM contributions WHERE contributor_id = ? AND coderabbit_score IS NOT NULL",
            )
            .bind(&contrib.id)
            .fetch_one(&state.db_pool)
            .await
            .ok()
            .flatten()
            .unwrap_or(0.0);

            HttpResponse::Ok().json(serde_json::json!({
                "contributor": {
                    "github_username": contrib.github_username,
                    "total_merit_points": contrib.total_merit_points,
                    "current_role": contrib.current_role,
                    "contributions_count": contrib.contributions_count,
                    "member_since": contrib.created_at,
                },
                "stats": {
                    "merged_contributions": merged_count,
                    "average_coderabbit_score": avg_score,
                    "recent_activities": activities,
                }
            }))
        }
        Ok(None) => HttpResponse::NotFound().json(serde_json::json!({
            "error": "Contributor not found"
        })),
        Err(e) => {
            error!("Database error: {}", e);
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal server error"
            }))
        }
    }
}

#[get("/api/leaderboard")]
pub async fn get_leaderboard(state: web::Data<AppState>) -> impl Responder {
    let registry = foundry_registry(&state);
    // Public: private repositories are combined, never named.
    let q = LeaderboardQuery { repo: None, limit: 25, include_private_names: false };
    match leaderboard::leaderboard(&state.db_pool, &registry, q).await {
        Ok(rows) => HttpResponse::Ok().json(serde_json::json!({
            "leaderboard": public_leaderboard_json(&rows),
            "generated_at": chrono::Utc::now(),
        })),
        Err(e) => {
            error!("Failed to fetch leaderboard: {}", e);
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to fetch leaderboard"
            }))
        }
    }
}

/// The public leaderboard row shape: the existing fields plus `by_repo`.
pub fn public_leaderboard_json(rows: &[leaderboard::LeaderboardRow]) -> Vec<serde_json::Value> {
    rows.iter()
        .map(|r| {
            serde_json::json!({
                "rank": r.rank,
                "github_username": r.github_username,
                "total_merit_points": r.total_points,
                "current_role": r.current_role,
                "contributions_count": r.contributions_count,
                "by_repo": r.by_repo,
            })
        })
        .collect()
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|h| h.to_str().ok())
}

/// GitLab webhook core, separated from actix state so it can be tested directly.
/// Order: token (constant time) -> parse -> normalize -> registry -> delivery dedupe -> merit.
pub async fn process_gitlab_webhook(
    pool: &SqlitePool,
    secret: Option<&str>,
    registry: Arc<RepoRegistry>,
    payments: Arc<PaymentConfig>,
    rules: Arc<MeritRules>,
    headers: &HeaderMap,
    body: &[u8],
) -> (StatusCode, serde_json::Value) {
    process_gitlab_webhook_with_cra(pool, secret, registry, payments, rules, None, headers, body).await
}

/// [`process_gitlab_webhook`] plus CRA loading: a verified `Push Hook` to the CRA
/// repository's configured branch runs the loader for the pushed head SHA. Every other
/// hook takes exactly the merit path above.
#[allow(clippy::too_many_arguments)]
pub async fn process_gitlab_webhook_with_cra(
    pool: &SqlitePool,
    secret: Option<&str>,
    registry: Arc<RepoRegistry>,
    payments: Arc<PaymentConfig>,
    rules: Arc<MeritRules>,
    cra: Option<&CraContext>,
    headers: &HeaderMap,
    body: &[u8],
) -> (StatusCode, serde_json::Value) {
    if !foundry::gitlab::verify_token(secret, header(headers, "X-Gitlab-Token")) {
        error!("Rejected GitLab webhook: missing or invalid X-Gitlab-Token");
        return (StatusCode::UNAUTHORIZED, serde_json::json!({"status": "error", "message": "invalid token"}));
    }
    let payload: foundry::gitlab::GitLabWebhookPayload = match serde_json::from_slice(body) {
        Ok(p) => p,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, serde_json::json!({"status": "error", "message": format!("invalid payload: {}", e)}));
        }
    };
    let delivery = header(headers, "X-Gitlab-Event-UUID")
        .or_else(|| header(headers, "Idempotency-Key"))
        .map(str::trim)
        .filter(|d| !d.is_empty());
    if payload.object_kind == "push" {
        if let Some(ctx) = cra {
            if let Some(r) = gitlab_cra_push(ctx, body, delivery).await {
                return match r {
                    Ok(o) => (StatusCode::OK, serde_json::json!({"status": o.label(), "cra": cra_json(&Ok(o.clone()))})),
                    Err(e) => {
                        error!("CRA load failed: {}", e);
                        (StatusCode::INTERNAL_SERVER_ERROR, serde_json::json!({"status": "error", "message": "CRA load failed"}))
                    }
                };
            }
        }
    }
    let ev = match foundry::gitlab::normalize(&payload, delivery) {
        Ok(Some(ev)) => ev,
        Ok(None) => return (StatusCode::OK, serde_json::json!({"status": "ignored", "reason": "not a merit event"})),
        Err(e) => return (StatusCode::BAD_REQUEST, serde_json::json!({"status": "error", "message": e})),
    };
    let processor = FoundryProcessor::new(pool.clone()).with_payments(payments).with_registry(registry).with_rules(rules);
    let decision = processor.decide(&ev);
    if decision != RepoDecision::Earn {
        warn!("GitLab event for {:?} on {} earns nothing: {:?}", ev.repo, ev.host, decision);
        return (StatusCode::OK, serde_json::json!({"status": "ignored", "reason": format!("{:?}", decision)}));
    }
    let delivery_key = delivery.map(|d| foundry::gitlab::delivery_key(&ev.host, d));
    if let Some(key) = &delivery_key {
        match crate::github::dedupe::claim(pool, key).await {
            Ok(true) => {}
            Ok(false) => {
                info!("Duplicate GitLab delivery {}; ignoring", key);
                return (StatusCode::OK, serde_json::json!({"status": "duplicate"}));
            }
            Err(e) => {
                error!("dedupe claim failed: {}", e);
                return (StatusCode::INTERNAL_SERVER_ERROR, serde_json::json!({"status": "error"}));
            }
        }
    }
    match processor.process(&ev).await {
        Ok(outcome) => {
            let status = match outcome {
                ProcessOutcome::Applied { .. } => "success",
                ProcessOutcome::Duplicate => "duplicate",
                ProcessOutcome::Ignored(_) => "ignored",
            };
            (StatusCode::OK, serde_json::json!({"status": status}))
        }
        Err(e) => {
            error!("Failed to handle GitLab webhook: {}", e);
            if let Some(key) = &delivery_key {
                let _ = crate::github::dedupe::release(pool, key).await;
            }
            (StatusCode::INTERNAL_SERVER_ERROR, serde_json::json!({"status": "error", "message": e.to_string()}))
        }
    }
}

/// The CRA side of a verified GitLab push hook. `None` unless it is a branch push to
/// the CRA repository's configured branch.
pub async fn gitlab_cra_push(cra: &CraContext, body: &[u8], delivery: Option<&str>) -> Option<Result<LoadOutcome, CraRuntimeError>> {
    let push: foundry::gitlab::GitLabPushPayload = match serde_json::from_slice(body) {
        Ok(p) => p,
        Err(e) => {
            warn!("GitLab push hook did not parse: {}", e);
            return None;
        }
    };
    let ev = match foundry::gitlab::normalize_push(&push, delivery) {
        Ok(Some(ev)) => ev,
        Ok(None) => return None,
        Err(e) => {
            warn!("GitLab push hook refused: {}", e);
            return None;
        }
    };
    cra.on_push(&ev, chrono::Utc::now().date_naive()).await
}

#[post("/gitlab/webhook")]
pub async fn gitlab_webhook(req: HttpRequest, body: web::Bytes, state: web::Data<AppState>) -> impl Responder {
    let (status, json) = process_gitlab_webhook_with_cra(
        &state.db_pool,
        state.config.gitlab.webhook_secret.as_deref(),
        foundry_registry(&state),
        state.payments.clone(),
        merit_rules(&state),
        Some(state.cra.as_ref()),
        req.headers(),
        &body,
    )
    .await;
    state.cra.refresh_merits().await;
    HttpResponse::build(status).json(json)
}

#[cfg(test)]
mod signature_tests {
    use super::verify_github_signature;

    // Example from GitHub docs: secret "It's a Secret to Everybody", payload "Hello, World!"
    const SECRET: &[u8] = b"It's a Secret to Everybody";
    const BODY: &[u8] = b"Hello, World!";
    const SIG: &str = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";

    #[test]
    fn accepts_valid_signature() {
        assert!(verify_github_signature(SECRET, BODY, Some(SIG)));
    }

    #[test]
    fn rejects_invalid_or_missing_signature() {
        assert!(!verify_github_signature(SECRET, b"tampered", Some(SIG)));
        assert!(!verify_github_signature(SECRET, BODY, None));
        assert!(!verify_github_signature(SECRET, BODY, Some("sha1=abc")));
        assert!(!verify_github_signature(b"", BODY, Some(SIG)));
    }
}

#[cfg(test)]
mod gitlab_webhook_tests {
    use super::*;
    use crate::foundry::gitlab::tests::mr_hook;
    use actix_web::test::TestRequest;
    use sqlx::sqlite::SqlitePoolOptions;

    const SECRET: &str = "gl-test-secret";

    async fn pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn headers(token: Option<&str>, uuid: Option<&str>, idem: Option<&str>) -> HeaderMap {
        let mut req = TestRequest::post().insert_header(("X-Gitlab-Event", "Merge Request Hook"));
        if let Some(t) = token {
            req = req.insert_header(("X-Gitlab-Token", t));
        }
        if let Some(u) = uuid {
            req = req.insert_header(("X-Gitlab-Event-UUID", u));
        }
        if let Some(i) = idem {
            req = req.insert_header(("Idempotency-Key", i));
        }
        req.to_http_request().headers().clone()
    }

    fn body(action: &str, path: &str) -> Vec<u8> {
        serde_json::to_vec(&mr_hook(action, (5, "carol"), 5, path, &format!("https://gitlab.com/{path}"))).unwrap()
    }

    async fn call(pool: &SqlitePool, secret: Option<&str>, spec: &str, h: &HeaderMap, b: &[u8]) -> (StatusCode, serde_json::Value) {
        let reg = Arc::new(RepoRegistry::from_spec(spec).unwrap());
        let pay = Arc::new(PaymentConfig::from_lookup(|_| None));
        process_gitlab_webhook(pool, secret, reg, pay, Arc::new(MeritRules::default()), h, b).await
    }

    /// (sum of merit, merit_activities, contributions, payout_ledger, processed keys)
    async fn counts(pool: &SqlitePool) -> (i64, i64, i64, i64, i64) {
        let c = |q: &'static str| async move { sqlx::query_scalar::<_, i64>(q).fetch_one(pool).await.unwrap() };
        (
            c("SELECT COALESCE(SUM(total_merit_points), 0) FROM contributors").await,
            c("SELECT COUNT(*) FROM merit_activities").await,
            c("SELECT COUNT(*) FROM contributions").await,
            c("SELECT COUNT(*) FROM payout_ledger").await,
            c("SELECT COUNT(*) FROM processed_webhook_events").await,
        )
    }

    const LISTED: &str = "gitlab:gitlab.com:grp/proj";

    #[tokio::test]
    async fn wrong_missing_and_empty_tokens_are_rejected() {
        let pool = pool().await;
        let b = body("merge", "grp/proj");
        for (secret, token) in [
            (Some(SECRET), Some("wrong")),
            (Some(SECRET), None),
            (Some(SECRET), Some("")),
            (Some(""), Some("")),   // empty secret rejects everything
            (None, Some("")),       // unset secret rejects everything
            (None, Some(SECRET)),
        ] {
            let (st, _) = call(&pool, secret, LISTED, &headers(token, Some("u-1"), None), &b).await;
            assert_eq!(st, StatusCode::UNAUTHORIZED, "secret {secret:?} token {token:?}");
        }
        assert_eq!(counts(&pool).await, (0, 0, 0, 0, 0), "a rejected webhook must write nothing");
        // Control: the right token is accepted
        let (st, _) = call(&pool, Some(SECRET), LISTED, &headers(Some(SECRET), Some("u-1"), None), &b).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(counts(&pool).await.0, 10);
    }

    #[tokio::test]
    async fn replays_are_deduped_by_uuid_idempotency_key_and_effect_key() {
        let pool = pool().await;
        let ok = |uuid: Option<&'static str>, idem: Option<&'static str>| headers(Some(SECRET), uuid, idem);
        let open = body("open", "grp/proj");
        let merge = body("merge", "grp/proj");

        let (st, j) = call(&pool, Some(SECRET), LISTED, &ok(Some("u-open"), None), &open).await;
        assert_eq!((st, j["status"].as_str()), (StatusCode::OK, Some("success")));
        let (st, j) = call(&pool, Some(SECRET), LISTED, &ok(Some("u-merge"), None), &merge).await;
        assert_eq!((st, j["status"].as_str()), (StatusCode::OK, Some("success")));
        let after = counts(&pool).await;
        assert_eq!((after.0, after.1, after.2, after.3), (15, 2, 1, 1));

        // Same UUID replayed
        let (_, j) = call(&pool, Some(SECRET), LISTED, &ok(Some("u-merge"), None), &merge).await;
        assert_eq!(j["status"], "duplicate");
        // Idempotency-Key fallback replayed
        let (_, j) = call(&pool, Some(SECRET), LISTED, &ok(None, Some("k-1")), &merge).await;
        assert_eq!(j["status"], "duplicate", "fresh delivery key but merge effect already applied");
        let (_, j) = call(&pool, Some(SECRET), LISTED, &ok(None, Some("k-1")), &merge).await;
        assert_eq!(j["status"], "duplicate");
        // No delivery id at all: effect key still dedupes
        let (_, j) = call(&pool, Some(SECRET), LISTED, &ok(None, None), &merge).await;
        assert_eq!(j["status"], "duplicate");
        let now = counts(&pool).await;
        assert_eq!((now.0, now.1, now.2, now.3), (15, 2, 1, 1), "replays changed merit or ledger");

        let keys: Vec<String> = sqlx::query_scalar("SELECT event_key FROM processed_webhook_events ORDER BY event_key").fetch_all(&pool).await.unwrap();
        assert!(keys.iter().all(|k| k.starts_with("gitlab:gitlab.com:")), "{keys:?}");
        assert!(keys.contains(&"gitlab:gitlab.com:delivery:u-merge".to_string()));
        assert!(keys.contains(&"gitlab:gitlab.com:delivery:k-1".to_string()));
        assert!(keys.contains(&"gitlab:gitlab.com:pr_merged:id555:deadbeef".to_string()));
    }

    #[tokio::test]
    async fn unlisted_mirror_and_unset_registry_earn_nothing() {
        for spec in [
            "gitlab:gitlab.com:grp/other",        // unlisted
            "gitlab:gitlab.com:grp/proj:mirror",  // mirror
            "",                                   // registry unset: GitLab refused
            "github:github.com:grp/proj",         // same path, other provider
        ] {
            let pool = pool().await;
            for action in ["open", "merge", "approved"] {
                let (st, j) = call(&pool, Some(SECRET), spec, &headers(Some(SECRET), None, None), &body(action, "grp/proj")).await;
                assert_eq!((st, j["status"].as_str()), (StatusCode::OK, Some("ignored")), "{spec} {action}");
            }
            assert_eq!(counts(&pool).await, (0, 0, 0, 0, 0), "{spec}: must earn nothing and create no ledger row");
        }
    }

    #[tokio::test]
    async fn self_approval_on_gitlab_earns_nothing_but_peer_approval_does() {
        let pool = pool().await;
        let h = headers(Some(SECRET), None, None);
        call(&pool, Some(SECRET), LISTED, &h, &body("open", "grp/proj")).await;
        let (_, j) = call(&pool, Some(SECRET), LISTED, &h, &body("approved", "grp/proj")).await;
        assert_eq!(j["status"], "ignored");
        let peer = serde_json::to_vec(&mr_hook("approved", (9, "maint"), 5, "grp/proj", "https://gitlab.com/grp/proj")).unwrap();
        let (_, j) = call(&pool, Some(SECRET), LISTED, &h, &peer).await;
        assert_eq!(j["status"], "success");
        let maint: i64 = sqlx::query_scalar("SELECT total_merit_points FROM contributors WHERE github_username = 'gitlab:maint'").fetch_one(&pool).await.unwrap();
        assert_eq!(maint, 10);
    }

    #[tokio::test]
    async fn merge_by_maintainer_pays_the_recorded_author() {
        let pool = pool().await;
        let h = headers(Some(SECRET), None, None);
        call(&pool, Some(SECRET), LISTED, &h, &body("open", "grp/proj")).await;
        let by_maint = serde_json::to_vec(&mr_hook("merge", (9, "maint"), 5, "grp/proj", "https://gitlab.com/grp/proj")).unwrap();
        let (_, j) = call(&pool, Some(SECRET), LISTED, &h, &by_maint).await;
        assert_eq!(j["status"], "success");
        let rows: Vec<(String, i64)> = sqlx::query_as("SELECT github_username, total_merit_points FROM contributors").fetch_all(&pool).await.unwrap();
        assert_eq!(rows, vec![("gitlab:carol".to_string(), 15)]);
        let (user, pr_id): (String, i64) = sqlx::query_as("SELECT github_username, pr_id FROM payout_ledger").fetch_one(&pool).await.unwrap();
        assert_eq!((user.as_str(), pr_id), ("gitlab:carol", 555));
    }
}
