use super::ledger::{self, LedgerError};
use super::onboarding::{self, OnboardError};
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

fn onboard_error(e: OnboardError) -> HttpResponse {
    match e {
        OnboardError::UnknownContributor => HttpResponse::NotFound().json(json!({"status": "error", "message": e.to_string()})),
        OnboardError::NotLinked => HttpResponse::UnprocessableEntity().json(json!({"status": "error", "message": e.to_string()})),
        OnboardError::Stripe(_) => {
            error!("{}", e);
            HttpResponse::BadGateway().json(json!({"status": "error", "message": e.to_string()}))
        }
        OnboardError::Db(err) => ledger_error(err.into()),
    }
}

/// Create (or reuse) a Stripe Express account for a linked contributor and return an onboarding link.
/// Admin-token protected (server-to-server). Contributors use the /payout-setup Discord command.
#[post("/api/payouts/onboard")]
pub async fn onboard(req: HttpRequest, body: web::Json<OnboardRequest>, state: web::Data<AppState>) -> HttpResponse {
    if let Err(r) = require_admin(&req, &state) {
        return r;
    }
    match onboarding::onboarding_link_for_github(&state.db_pool, state.stripe.as_ref(), &state.payments, &body.github_username).await {
        Ok(link) => HttpResponse::Ok().json(json!({"status": "success", "account_id": link.account_id, "onboarding_url": link.url})),
        Err(e) => onboard_error(e),
    }
}

fn html_page(title: &str, message: &str) -> HttpResponse {
    HttpResponse::Ok().content_type("text/html; charset=utf-8").body(format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{t}</title>\
         <style>body{{font-family:system-ui,sans-serif;max-width:36rem;margin:4rem auto;padding:0 1rem;line-height:1.5}}</style>\
         </head><body><h1>{t}</h1><p>{m}</p></body></html>",
        t = title,
        m = message
    ))
}

/// Stripe sends the contributor here after onboarding. Status arrives separately via account.updated.
#[get("/payouts/return")]
pub async fn payouts_return() -> HttpResponse {
    html_page(
        "Payout setup submitted",
        "Thanks! Stripe is reviewing your details. Once your account is verified you will be eligible for approved contribution payouts. You can close this tab. If Stripe asked for more information, run /payout-setup in Discord again.",
    )
}

#[derive(Debug, Deserialize)]
pub struct RefreshQuery {
    pub account: Option<String>,
    pub sig: Option<String>,
}

/// Stripe sends the contributor here when an onboarding link expired. Regenerates a link and redirects.
#[get("/payouts/refresh")]
pub async fn payouts_refresh(q: web::Query<RefreshQuery>, state: web::Data<AppState>) -> HttpResponse {
    let expired = || html_page("Link expired", "This setup link is no longer valid. Run /payout-setup in Discord to get a new one.");
    let (Some(account), Some(sig)) = (q.account.as_deref(), q.sig.as_deref()) else {
        return expired();
    };
    if !onboarding::verify_refresh_sig(&state.payments, account, sig) {
        warn!("Rejected payout refresh with bad signature");
        return expired();
    }
    let known: Option<i64> = sqlx::query_scalar("SELECT 1 FROM contributors WHERE stripe_account_id = ?")
        .bind(account)
        .fetch_optional(&state.db_pool)
        .await
        .ok()
        .flatten();
    if known.is_none() {
        return expired();
    }
    match onboarding::link_for_account(state.stripe.as_ref(), &state.payments, account).await {
        Ok(url) => HttpResponse::Found().append_header(("Location", url)).finish(),
        Err(e) => {
            error!("refresh link failed: {}", e);
            html_page("Something went wrong", "Could not create a new setup link. Please try /payout-setup in Discord.")
        }
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