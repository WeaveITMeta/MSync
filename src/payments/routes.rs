use super::ledger::{self, LedgerError};
use super::signature::{token_matches, verify_stripe_signature, STRIPE_TOLERANCE_SECS};
use crate::AppState;
use actix_web::{get, post, web, HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;
use tracing::{error, info, warn};

/// Require `Authorization: Bearer <DSYNC_ADMIN_TOKEN>`. Disabled (503) if the token is unset.
fn require_admin(req: &HttpRequest, state: &AppState) -> Result<(), HttpResponse> {
    let Some(expected) = state.payments.admin_token.as_deref() else {
        return Err(HttpResponse::ServiceUnavailable().json(json!({"status": "error", "message": "admin API disabled (DSYNC_ADMIN_TOKEN not set)"})));
    };
    let provided = req
        .headers()
        .get("Authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("");
    if token_matches(expected, provided.trim()) {
        Ok(())
    } else {
        Err(HttpResponse::Unauthorized().json(json!({"status": "error", "message": "unauthorized"})))
    }
}

fn ledger_error(e: LedgerError) -> HttpResponse {
    match e {
        LedgerError::NotFound => HttpResponse::NotFound().json(json!({"status": "error", "message": e.to_string()})),
        LedgerError::InvalidState(..) => HttpResponse::Conflict().json(json!({"status": "error", "message": e.to_string()})),
        LedgerError::NotPayable(_) => HttpResponse::UnprocessableEntity().json(json!({"status": "error", "message": e.to_string()})),
        LedgerError::Db(err) => {
            error!("ledger db error: {}", err);
            HttpResponse::InternalServerError().json(json!({"status": "error", "message": "database error"}))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct OnboardRequest {
    pub github_username: String,
}

/// Create (or reuse) a Stripe Express account for a linked contributor and return an onboarding link.
/// Admin-token protected: intended to be called by the website/bot backend on the contributor's behalf.
#[post("/api/payouts/onboard")]
pub async fn onboard(req: HttpRequest, body: web::Json<OnboardRequest>, state: web::Data<AppState>) -> HttpResponse {
    if let Err(r) = require_admin(&req, &state) {
        return r;
    }
    let row: Option<(Option<i64>, Option<String>)> = match sqlx::query_as(
        "SELECT discord_user_id, stripe_account_id FROM contributors WHERE github_username = ?",
    )
    .bind(&body.github_username)
    .fetch_optional(&state.db_pool)
    .await
    {
        Ok(r) => r,
        Err(e) => return ledger_error(e.into()),
    };
    let existing = match row {
        None => return HttpResponse::NotFound().json(json!({"status": "error", "message": "unknown contributor"})),
        Some((None, _)) => {
            return HttpResponse::UnprocessableEntity().json(json!({"status": "error", "message": "contributor has not linked Discord (/link)"}))
        }
        Some((Some(_), acct)) => acct.filter(|a| !a.is_empty()),
    };

    let account_id = match existing {
        Some(a) => a,
        None => match state.stripe.create_express_account(&body.github_username).await {
            Ok(a) => {
                if let Err(e) = ledger::set_stripe_account(&state.db_pool, &body.github_username, &a).await {
                    return ledger_error(e.into());
                }
                a
            }
            Err(e) => {
                error!("Stripe account creation failed: {}", e);
                return HttpResponse::BadGateway().json(json!({"status": "error", "message": e.to_string()}));
            }
        },
    };

    let base = &state.payments.public_url;
    match state
        .stripe
        .create_account_link(&account_id, &format!("{}/payouts/refresh", base), &format!("{}/payouts/return", base))
        .await
    {
        Ok(url) => HttpResponse::Ok().json(json!({"status": "success", "account_id": account_id, "onboarding_url": url})),
        Err(e) => HttpResponse::BadGateway().json(json!({"status": "error", "message": e.to_string()})),
    }
}

/// Stripe webhook receiver. Verifies Stripe-Signature with STRIPE_WEBHOOK_SECRET.
/// Handles account.updated to record whether a connected account can receive transfers.
#[post("/stripe/webhook")]
pub async fn stripe_webhook(req: HttpRequest, body: web::Bytes, state: web::Data<AppState>) -> HttpResponse {
    let secret = state.payments.stripe_webhook_secret.as_deref().unwrap_or("");
    let header = req.headers().get("Stripe-Signature").and_then(|h| h.to_str().ok());
    if !verify_stripe_signature(secret, &body, header, chrono::Utc::now().timestamp(), STRIPE_TOLERANCE_SECS) {
        warn!("Rejected Stripe webhook: invalid signature");
        return HttpResponse::Unauthorized().json(json!({"status": "error", "message": "invalid signature"}));
    }
    let event: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return HttpResponse::BadRequest().json(json!({"status": "error", "message": "invalid payload"})),
    };

    if event["type"] == "account.updated" {
        let obj = &event["data"]["object"];
        let Some(account_id) = obj["id"].as_str() else {
            return HttpResponse::BadRequest().json(json!({"status": "error", "message": "missing account id"}));
        };
        let enabled = account_transfers_enabled(obj);
        match ledger::update_account_status(&state.db_pool, account_id, enabled).await {
            Ok(n) => info!("account.updated {} transfers_enabled={} ({} contributor rows)", account_id, enabled, n),
            Err(e) => return ledger_error(e.into()),
        }
    }
    HttpResponse::Ok().json(json!({"received": true}))
}

/// A connected account can be paid when payouts are enabled and the transfers capability is active.
pub fn account_transfers_enabled(account: &serde_json::Value) -> bool {
    account["payouts_enabled"].as_bool().unwrap_or(false)
        && account["capabilities"]["transfers"].as_str() == Some("active")
}

#[derive(Debug, Deserialize)]
pub struct LedgerQuery {
    pub status: Option<String>,
}

#[get("/api/admin/ledger")]
pub async fn list_ledger(req: HttpRequest, q: web::Query<LedgerQuery>, state: web::Data<AppState>) -> HttpResponse {
    if let Err(r) = require_admin(&req, &state) {
        return r;
    }
    match ledger::list(&state.db_pool, q.status.as_deref()).await {
        Ok(rows) => HttpResponse::Ok().json(json!({"status": "success", "entries": rows})),
        Err(e) => ledger_error(e.into()),
    }
}

#[derive(Debug, Deserialize)]
pub struct DecisionRequest {
    pub decided_by: String,
    pub reason: Option<String>,
}

#[post("/api/admin/ledger/{id}/approve")]
pub async fn approve_entry(req: HttpRequest, path: web::Path<String>, body: web::Json<DecisionRequest>, state: web::Data<AppState>) -> HttpResponse {
    if let Err(r) = require_admin(&req, &state) {
        return r;
    }
    match ledger::approve(&state.db_pool, state.stripe.as_ref(), &path, &body.decided_by).await {
        Ok(entry) => HttpResponse::Ok().json(json!({"status": "success", "entry": entry})),
        Err(e) => ledger_error(e),
    }
}

#[post("/api/admin/ledger/{id}/reject")]
pub async fn reject_entry(req: HttpRequest, path: web::Path<String>, body: web::Json<DecisionRequest>, state: web::Data<AppState>) -> HttpResponse {
    if let Err(r) = require_admin(&req, &state) {
        return r;
    }
    let reason = body.reason.as_deref().unwrap_or("rejected by admin");
    match ledger::reject(&state.db_pool, &path, &body.decided_by, reason).await {
        Ok(entry) => HttpResponse::Ok().json(json!({"status": "success", "entry": entry})),
        Err(e) => ledger_error(e),
    }
}