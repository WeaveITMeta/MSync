//! `POST /api/v1/tokens` (tokens.manage, or the legacy MSYNC_ADMIN_TOKEN as bootstrap) and
//! `POST /api/v1/tokens/{id}/revoke`.

use super::tokens::{self, NewToken, TokenCreator, TokenError};
use crate::payments::api::{auth_error, unauthorized, ApiContext};
use crate::payments::signature::token_matches;
use actix_web::{post, web, HttpRequest, HttpResponse};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;
use tracing::{error, warn};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTokenBody {
    pub label: String,
    pub member_id: String,
    pub scopes: Vec<String>,
    #[serde(default)]
    pub payee_keys: Vec<String>,
    /// RFC 3339, optional.
    pub expires_at: Option<DateTime<Utc>>,
}

enum Caller {
    Bootstrap,
    Token(tokens::Principal),
}

/// The bootstrap admin token, or an API token. Every failure is the same 401.
async fn caller(req: &HttpRequest, ctx: &ApiContext) -> Result<Caller, HttpResponse> {
    let Some(presented) = tokens::bearer(req) else {
        return Err(unauthorized());
    };
    if let Some(admin) = ctx.payments.admin_token.as_deref() {
        if token_matches(admin, presented) {
            return Ok(Caller::Bootstrap);
        }
    }
    match tokens::authenticate_token(&ctx.pool, presented, Utc::now()).await {
        Ok(p) => Ok(Caller::Token(p)),
        Err(e) => {
            warn!("token API auth refused: {}", e);
            Err(auth_error(&e))
        }
    }
}

fn token_error(e: TokenError) -> HttpResponse {
    match e {
        TokenError::Auth(a) => auth_error(&a),
        TokenError::Forbidden(_) => {
            HttpResponse::Forbidden().json(json!({"status": "error", "message": e.to_string()}))
        }
        TokenError::Invalid(_) => {
            HttpResponse::BadRequest().json(json!({"status": "error", "message": e.to_string()}))
        }
        TokenError::NotFound => {
            HttpResponse::NotFound().json(json!({"status": "error", "message": "token not found"}))
        }
        TokenError::Rng => {
            error!("token creation: random source failed");
            HttpResponse::InternalServerError()
                .json(json!({"status": "error", "message": "could not create token"}))
        }
        TokenError::Db(d) => {
            error!("token db error: {}", d);
            HttpResponse::InternalServerError()
                .json(json!({"status": "error", "message": "database error"}))
        }
    }
}

#[post("/api/v1/tokens")]
pub async fn create_token(
    req: HttpRequest,
    body: web::Json<CreateTokenBody>,
    ctx: web::Data<ApiContext>,
) -> HttpResponse {
    let caller = match caller(&req, &ctx).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let body = body.into_inner();
    let new = NewToken {
        label: body.label,
        member_id: body.member_id,
        scopes: body.scopes,
        payee_keys: body.payee_keys,
        expires_at: body.expires_at,
    };
    let creator = match &caller {
        Caller::Bootstrap => TokenCreator::Bootstrap,
        Caller::Token(p) => TokenCreator::Principal(p),
    };
    match tokens::create_token(&ctx.pool, ctx.directory.as_ref(), creator, new, Utc::now()).await {
        Ok(t) => HttpResponse::Created().json(json!({
            "status": "success",
            "note": "the token is shown once; store it now",
            "token": t,
        })),
        Err(e) => token_error(e),
    }
}

#[post("/api/v1/tokens/{id}/revoke")]
pub async fn revoke_token(
    req: HttpRequest,
    path: web::Path<String>,
    ctx: web::Data<ApiContext>,
) -> HttpResponse {
    let caller = match caller(&req, &ctx).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let creator = match &caller {
        Caller::Bootstrap => TokenCreator::Bootstrap,
        Caller::Token(p) => TokenCreator::Principal(p),
    };
    match tokens::revoke_token(
        &ctx.pool,
        ctx.directory.as_ref(),
        creator,
        &path,
        Utc::now(),
    )
    .await
    {
        Ok(()) => {
            HttpResponse::Ok().json(json!({"status": "success", "revoked": path.into_inner()}))
        }
        Err(e) => token_error(e),
    }
}
