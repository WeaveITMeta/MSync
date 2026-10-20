use crate::github::webhook::{GitHubWebhookHandler, GitHubWebhookPayload};
use crate::models::{ContributorStats, LeaderboardEntry};
use crate::AppState;
use actix_web::{get, post, web, HttpRequest, HttpResponse, Responder};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::{error, info};

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub uptime_seconds: u64,
}

#[derive(Debug, Deserialize)]
pub struct StatsQuery {
    pub github_username: String,
}

#[get("/health")]
pub async fn health_check() -> impl Responder {
    HttpResponse::Ok().json(HealthResponse {
        status: "healthy".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_seconds: 0, // Could implement actual uptime tracking
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

    let handler = GitHubWebhookHandler::new(
        state.db_pool.clone(),
        state.config.coderabbit.api_key.clone(),
    );

    match handler.handle_webhook(event_type, payload).await {
        Ok(_) => HttpResponse::Ok().json(serde_json::json!({
            "status": "success",
            "event": event_type
        })),
        Err(e) => {
            error!("Failed to handle webhook: {}", e);
            HttpResponse::InternalServerError().json(serde_json::json!({
                "status": "error",
                "message": e.to_string()
            }))
        }
    }
}

#[get("/api/contributor/stats")]
pub async fn get_contributor_stats(
    query: web::Query<StatsQuery>,
    state: web::Data<AppState>,
) -> impl Responder {
    let contributor = sqlx::query!(
        "SELECT * FROM contributors WHERE github_username = ?",
        query.github_username
    )
    .fetch_optional(&state.db_pool)
    .await;

    match contributor {
        Ok(Some(contrib)) => {
            // Get recent activities
            let activities = sqlx::query!(
                "SELECT * FROM merit_activities WHERE contributor_id = ? 
                 ORDER BY created_at DESC LIMIT 10",
                contrib.id
            )
            .fetch_all(&state.db_pool)
            .await
            .unwrap_or_default();

            // Calculate statistics
            let merged_count = sqlx::query!(
                "SELECT COUNT(*) as count FROM contributions 
                 WHERE contributor_id = ? AND status = 'Merged'",
                contrib.id
            )
            .fetch_one(&state.db_pool)
            .await
            .map(|r| r.count)
            .unwrap_or(0);

            let avg_score = sqlx::query!(
                "SELECT AVG(coderabbit_score) as avg_score FROM contributions 
                 WHERE contributor_id = ? AND coderabbit_score IS NOT NULL",
                contrib.id
            )
            .fetch_one(&state.db_pool)
            .await
            .ok()
            .and_then(|r| r.avg_score)
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
                    "recent_activities": activities.len(),
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
    let contributors = sqlx::query!(
        "SELECT * FROM contributors ORDER BY total_merit_points DESC LIMIT 25"
    )
    .fetch_all(&state.db_pool)
    .await;

    match contributors {
        Ok(contributors) => {
            let leaderboard: Vec<_> = contributors
                .into_iter()
                .enumerate()
                .map(|(idx, c)| {
                    serde_json::json!({
                        "rank": idx + 1,
                        "github_username": c.github_username,
                        "total_merit_points": c.total_merit_points,
                        "current_role": c.current_role,
                        "contributions_count": c.contributions_count,
                    })
                })
                .collect();

            HttpResponse::Ok().json(serde_json::json!({
                "leaderboard": leaderboard,
                "generated_at": chrono::Utc::now(),
            }))
        }
        Err(e) => {
            error!("Failed to fetch leaderboard: {}", e);
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to fetch leaderboard"
            }))
        }
    }
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
