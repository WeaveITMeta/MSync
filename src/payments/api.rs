//! REST `/api/v1/payouts/*` (contract section 7). Thin handlers over the library functions in
//! `policy`, `approvals` and `runs`; the actor is always the token's member.

use super::approvals;
use super::config::PaymentConfig;
use super::error::PayoutError;
use super::ledger::LedgerError;
use super::policy::{self, PayoutPolicy, PolicyError};
use super::runs;
use super::stripe::StripeApi;
use crate::auth::{authenticate, AuthError, Directory, Principal};
use actix_web::{delete, get, post, put, web, HttpRequest, HttpResponse};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::Arc;
use tracing::{error, warn};

/// Everything the v1 API and the scheduler need. Registered as its own `web::Data` so the API can
/// be tested without the Discord/GitHub parts of `AppState`.
#[derive(Clone)]
pub struct ApiContext {
    pub pool: SqlitePool,
    pub payments: Arc<PaymentConfig>,
    pub stripe: Arc<dyn StripeApi>,
    pub directory: Arc<dyn Directory>,
}

/// The single 401 body used for every authentication failure.
pub fn unauthorized() -> HttpResponse {
    HttpResponse::Unauthorized().json(json!({"status": "error", "message": "unauthorized"}))
}

pub fn auth_error(e: &AuthError) -> HttpResponse {
    match e {
        AuthError::MissingScope(s) => {
            HttpResponse::Forbidden().json(json!({"status": "error", "message": format!("missing scope {s}")}))
        }
        AuthError::Db(err) => {
            error!("auth db error: {}", err);
            unauthorized()
        }
        _ => unauthorized(),
    }
}

pub async fn principal(req: &HttpRequest, ctx: &ApiContext) -> Result<Principal, HttpResponse> {
    authenticate(req, &ctx.pool).await.map_err(|e| {
        warn!("v1 auth refused: {}", e);
        auth_error(&e)
    })
}

fn err(status: actix_web::http::StatusCode, msg: impl Into<String>) -> HttpResponse {
    HttpResponse::build(status).json(json!({"status": "error", "message": msg.into()}))
}

pub fn payout_error(e: PayoutError) -> HttpResponse {
    use actix_web::http::StatusCode as S;
    match e {
        PayoutError::Auth(a) => auth_error(&a),
        PayoutError::NotFound(_) => err(S::NOT_FOUND, e.to_string()),
        PayoutError::InvalidState(_) => err(S::CONFLICT, e.to_string()),
        PayoutError::PayeeCannotApprove => err(S::FORBIDDEN, e.to_string()),
        PayoutError::Forbidden(_) => err(S::FORBIDDEN, e.to_string()),
        PayoutError::PolicyDisabled => err(S::CONFLICT, e.to_string()),
        PayoutError::Invalid(_) => err(S::BAD_REQUEST, e.to_string()),
        PayoutError::Policy(p) => policy_error(p),
        PayoutError::Ledger(l) => match l {
            LedgerError::NotFound => err(S::NOT_FOUND, l.to_string()),
            LedgerError::InvalidState(..) => err(S::CONFLICT, l.to_string()),
            LedgerError::NotPayable(_) => err(S::UNPROCESSABLE_ENTITY, l.to_string()),
            LedgerError::Policy(_) => {
                error!("{}", l);
                err(S::SERVICE_UNAVAILABLE, "payout policy unavailable")
            }
            LedgerError::Db(d) => {
                error!("ledger db error: {}", d);
                err(S::INTERNAL_SERVER_ERROR, "database error")
            }
        },
        PayoutError::Db(d) => {
            error!("payout db error: {}", d);
            err(S::INTERNAL_SERVER_ERROR, "database error")
        }
    }
}

pub fn policy_error(e: PolicyError) -> HttpResponse {
    use actix_web::http::StatusCode as S;
    match e {
        PolicyError::Auth(a) => auth_error(&a),
        PolicyError::Invalid(_) => err(S::BAD_REQUEST, e.to_string()),
        PolicyError::Conflict(_) => err(S::CONFLICT, e.to_string()),
        PolicyError::Corrupt(_) => {
            error!("{}", e);
            err(S::SERVICE_UNAVAILABLE, "stored payout policy is unusable")
        }
        PolicyError::Db(d) => {
            error!("policy db error: {}", d);
            err(S::INTERNAL_SERVER_ERROR, "database error")
        }
    }
}

macro_rules! auth_or_return {
    ($req:expr, $ctx:expr) => {
        match principal(&$req, &$ctx).await {
            Ok(p) => p,
            Err(r) => return r,
        }
    };
}

// ------------------------------------------------------------------ policy

#[get("/api/v1/payouts/policy")]
pub async fn get_policy(req: HttpRequest, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    match policy::get_policy(&ctx.pool, &ctx.payments, &p).await {
        Ok(eff) => HttpResponse::Ok().json(json!({
            "status": "success",
            "source": if eff.is_stored() { "stored" } else { "default" },
            "version": eff.version,
            "updated_by": eff.updated_by,
            "updated_at": eff.updated_at,
            "policy": eff.policy,
        })),
        Err(e) => policy_error(e),
    }
}

#[derive(Debug, Deserialize)]
pub struct PutPolicyQuery {
    /// Optimistic concurrency: refuse unless the current version equals this (0 = none stored yet).
    pub expected_version: Option<i64>,
}

#[put("/api/v1/payouts/policy")]
pub async fn put_policy(req: HttpRequest, q: web::Query<PutPolicyQuery>, body: web::Bytes, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    let parsed: PayoutPolicy = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(actix_web::http::StatusCode::BAD_REQUEST, format!("invalid policy: {e}")),
    };
    let expected = q.expected_version.map(|v| if v <= 0 { None } else { Some(v) });
    match policy::put_policy(&ctx.pool, &p, &parsed, expected).await {
        Ok(version) => HttpResponse::Ok().json(json!({"status": "success", "version": version, "policy": parsed})),
        Err(e) => policy_error(e),
    }
}

// ------------------------------------------------------------------ ledger

#[derive(Debug, Deserialize)]
pub struct LedgerQuery {
    pub status: Option<String>,
}

#[get("/api/v1/payouts/ledger")]
pub async fn list_ledger(req: HttpRequest, q: web::Query<LedgerQuery>, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    match runs::list_ledger(&ctx.pool, &p, q.status.as_deref()).await {
        Ok(rows) => HttpResponse::Ok().json(json!({"status": "success", "entries": rows})),
        Err(e) => payout_error(e),
    }
}

/// Approve one ledger row as the token's member. Any request body is ignored.
#[post("/api/v1/payouts/ledger/{id}/approve")]
pub async fn approve_ledger_row(req: HttpRequest, path: web::Path<String>, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    match approvals::approve_row(&ctx.pool, ctx.directory.as_ref(), &p, &path).await {
        Ok(r) => HttpResponse::Ok().json(json!({"status": "success", "result": r, "approver_member_id": p.member_id})),
        Err(e) => payout_error(e),
    }
}

// ------------------------------------------------------------------ runs

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRunBody {
    /// Defaults to true: a run is only persisted when the caller asks for it explicitly.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[post("/api/v1/payouts/runs")]
pub async fn create_run(req: HttpRequest, body: web::Json<CreateRunBody>, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    match runs::create_run(&ctx.pool, &ctx.payments, ctx.directory.as_ref(), &p, body.dry_run, Utc::now()).await {
        Ok(runs::CreateRunResult::DryRun(plan)) => HttpResponse::Ok().json(json!({"status": "success", "dry_run": true, "plan": plan})),
        Ok(runs::CreateRunResult::Created { run, plan }) => {
            HttpResponse::Created().json(json!({"status": "success", "dry_run": false, "run": run, "plan": plan}))
        }
        Err(e) => payout_error(e),
    }
}

#[get("/api/v1/payouts/runs/{id}")]
pub async fn get_run(req: HttpRequest, path: web::Path<String>, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    match runs::get_run(&ctx.pool, &p, &path).await {
        Ok(v) => HttpResponse::Ok().json(json!({"status": "success", "run": v})),
        Err(e) => payout_error(e),
    }
}

/// Approve every item of a run as the token's member. Any request body is ignored.
#[post("/api/v1/payouts/runs/{id}/approve")]
pub async fn approve_run(req: HttpRequest, path: web::Path<String>, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    match runs::approve_run(&ctx.pool, ctx.directory.as_ref(), &p, &path).await {
        Ok(r) => HttpResponse::Ok().json(json!({"status": "success", "report": r})),
        Err(e) => payout_error(e),
    }
}

#[post("/api/v1/payouts/runs/{id}/execute")]
pub async fn execute_run(req: HttpRequest, path: web::Path<String>, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    match runs::execute_run(&ctx.pool, ctx.stripe.as_ref(), &ctx.payments, ctx.directory.as_ref(), &p, &path, Utc::now()).await {
        Ok(r) => HttpResponse::Ok().json(json!({"status": "success", "report": r})),
        Err(e) => payout_error(e),
    }
}

#[post("/api/v1/payouts/runs/{id}/cancel")]
pub async fn cancel_run(req: HttpRequest, path: web::Path<String>, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    match runs::cancel_run(&ctx.pool, &p, &path, Utc::now()).await {
        Ok(v) => HttpResponse::Ok().json(json!({"status": "success", "run": v})),
        Err(e) => payout_error(e),
    }
}

// ------------------------------------------------------------------ blocks

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockBody {
    pub payee_key: String,
    pub reason: String,
}

#[post("/api/v1/payouts/blocks")]
pub async fn block_payee(req: HttpRequest, body: web::Json<BlockBody>, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    match approvals::block_payee(&ctx.pool, &p, &body.payee_key, &body.reason).await {
        Ok(()) => HttpResponse::Ok().json(json!({"status": "success"})),
        Err(e) => payout_error(e),
    }
}

#[delete("/api/v1/payouts/blocks/{payee_key}")]
pub async fn unblock_payee(req: HttpRequest, path: web::Path<String>, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    match approvals::unblock_payee(&ctx.pool, &p, &path).await {
        Ok(existed) => HttpResponse::Ok().json(json!({"status": "success", "removed": existed})),
        Err(e) => payout_error(e),
    }
}

/// Register every v1 route (tokens and payouts) on an app or scope.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(crate::auth::routes::create_token)
        .service(crate::auth::routes::revoke_token)
        .service(get_policy)
        .service(put_policy)
        .service(list_ledger)
        .service(approve_ledger_row)
        .service(create_run)
        .service(get_run)
        .service(approve_run)
        .service(execute_run)
        .service(cancel_run)
        .service(block_payee)
        .service(unblock_payee)
        .configure(super::payroll_api::configure);
}
