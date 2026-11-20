//! REST `GET /api/v1/audit` and `GET /api/v1/audit/verify` (`audit.read`, contract 13.1).

use actix_web::{get, web, HttpRequest, HttpResponse};
use serde_json::json;
use tracing::error;

use super::{list, verify, AuditFilter};
use crate::auth::scopes;
use crate::payments::api::{auth_error, principal, ApiContext};

fn db_error(e: sqlx::Error) -> HttpResponse {
    error!("audit db error: {e}");
    HttpResponse::InternalServerError().json(json!({"status": "error", "message": "database error"}))
}

/// `GET /api/v1/audit?actor=&action=&target=&after=&limit=`: one page, oldest first.
#[get("/api/v1/audit")]
pub async fn get_audit(req: HttpRequest, q: web::Query<AuditFilter>, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = match principal(&req, &ctx).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    if let Err(e) = p.require(scopes::AUDIT_READ) {
        return auth_error(&e);
    }
    match list(&ctx.pool, &q).await {
        Ok(page) => HttpResponse::Ok().json(json!({"status": "ok", "entries": page.entries, "next_after": page.next_after})),
        Err(e) => db_error(e),
    }
}

/// `GET /api/v1/audit/verify`: walk the chain and name the first broken entry.
#[get("/api/v1/audit/verify")]
pub async fn get_audit_verify(req: HttpRequest, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = match principal(&req, &ctx).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    if let Err(e) = p.require(scopes::AUDIT_READ) {
        return auth_error(&e);
    }
    match verify(&ctx.pool).await {
        Ok(report) => HttpResponse::Ok().json(json!({"status": "ok", "verify": report})),
        Err(e) => db_error(e),
    }
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    // `verify` first: `/api/v1/audit` and `/api/v1/audit/verify` are distinct paths anyway.
    cfg.service(get_audit_verify).service(get_audit);
}
