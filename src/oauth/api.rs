//! `POST /api/v1/oauth/clients` and `POST /api/v1/oauth/keys/rotate`: an owner registers a
//! platform and rotates the signing key. API bearer tokens only (scope `oauth.clients` held by
//! an owner, or the bootstrap `MSYNC_ADMIN_TOKEN`); a browser session never authorizes these.

use actix_web::{post, web, HttpRequest, HttpResponse};
use chrono::Utc;
use serde_json::json;
use tracing::error;

use super::clients::{self, ClientError, NewClient};
use super::{keys, OAuthContext, RETIRED_KEY_PUBLISH_SECS};
use crate::auth::{scopes, OWNER_RANK};
use crate::payments::api::{principal, ApiContext};
use crate::payments::signature::token_matches;

/// The acting owner: the bootstrap admin token, or a token holding `oauth.clients` whose member
/// has owner rank.
async fn owner(req: &HttpRequest, api: &ApiContext) -> Result<String, HttpResponse> {
    if let (Some(admin), Some(presented)) = (
        api.payments.admin_token.as_deref(),
        crate::auth::tokens::bearer(req),
    ) {
        if token_matches(admin, presented) {
            return Ok(crate::auth::LEGACY_ADMIN_MEMBER.to_string());
        }
    }
    if crate::accounts::mutant::on("api_accepts_session") {
        if let Some((s, _)) = crate::accounts::routes::session(req, &api.pool).await {
            return Ok(s.account_id);
        }
    }
    let p = principal(req, api).await?;
    if p.require(scopes::OAUTH_CLIENTS).is_err() {
        return Err(HttpResponse::Forbidden()
            .json(json!({"status": "error", "message": "missing scope oauth.clients"})));
    }
    if api.directory.rank_of(&p.member_id).unwrap_or(-1) < OWNER_RANK {
        return Err(HttpResponse::Forbidden()
            .json(json!({"status": "error", "message": "only an owner registers OAuth clients"})));
    }
    Ok(p.member_id)
}

#[post("/api/v1/oauth/clients")]
pub async fn create_client(
    req: HttpRequest,
    body: web::Json<NewClient>,
    api: web::Data<ApiContext>,
    ctx: web::Data<OAuthContext>,
) -> HttpResponse {
    let who = match owner(&req, &api).await {
        Ok(w) => w,
        Err(r) => return r,
    };
    let consumer_keys: Option<Vec<String>> = ctx.cra.active().map(|a| {
        a.snapshot
            .org
            .consumers
            .iter()
            .map(|c| c.key.clone())
            .collect()
    });
    match clients::register(
        &ctx.pool,
        body.into_inner(),
        &who,
        consumer_keys.as_deref(),
        Utc::now().timestamp(),
    )
    .await
    {
        Ok(c) => HttpResponse::Created().json(json!({
            "status": "success",
            "note": "the client secret is shown once; store it now",
            "client_id": c.client.client_id,
            "client_secret": c.client_secret,
            "client": c.client,
        })),
        Err(ClientError::Invalid(m)) => {
            HttpResponse::BadRequest().json(json!({"status": "error", "message": m}))
        }
        Err(e) => {
            error!("client registration failed: {}", e);
            HttpResponse::InternalServerError()
                .json(json!({"status": "error", "message": "could not register the client"}))
        }
    }
}

#[post("/api/v1/oauth/keys/rotate")]
pub async fn rotate_key(
    req: HttpRequest,
    api: web::Data<ApiContext>,
    ctx: web::Data<OAuthContext>,
) -> HttpResponse {
    if let Err(r) = owner(&req, &api).await {
        return r;
    }
    let Some(kek) = &ctx.kek else {
        return HttpResponse::ServiceUnavailable().json(
            json!({"status": "error", "message": "MSYNC_KEY_ENCRYPTION_KEY is not configured"}),
        );
    };
    match keys::rotate(
        &ctx.pool,
        kek,
        Utc::now().timestamp(),
        RETIRED_KEY_PUBLISH_SECS,
    )
    .await
    {
        Ok(kid) => HttpResponse::Ok().json(json!({"status": "success", "kid": kid})),
        Err(e) => {
            error!("key rotation failed: {}", e);
            HttpResponse::InternalServerError()
                .json(json!({"status": "error", "message": "key rotation failed"}))
        }
    }
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(create_client).service(rotate_key);
}
