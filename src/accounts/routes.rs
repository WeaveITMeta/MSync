//! Sign-in routes.
//!
//! ```text
//! GET  /auth/{provider}/start        ?next=/path  ?link=<link_nonce>
//! GET  /auth/{provider}/callback     state bound to this browser, single use
//! POST /auth/logout                  ?all=true ends every session of the account
//! POST /auth/email/start             {"email": "..."}  rate limited
//! GET  /auth/email/verify?token=     a confirm page; looking never uses the token
//! POST /auth/email/verify            token=...  single use
//! GET  /account                      the account, its identities, its CRA standing
//! POST /account/identities/{id}/unlink
//! ```
//!
//! Every state-changing cookie route checks `Origin` against `MSYNC_PUBLIC_URL`. Sessions are
//! read from the cookie only; an `Authorization` header is never a session.

use actix_web::cookie::{time::Duration as CookieDuration, Cookie, SameSite};
use actix_web::{get, post, web, HttpRequest, HttpResponse};
use base64::Engine;
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tracing::{error, warn};

use super::mutant;
use super::rules::{self, Provider};
use super::store::{self, AccountsError, EmailRequest, SessionInfo, UnlinkError};
use super::AccountsContext;

pub const SESSION_COOKIE: &str = "msync_session";
/// Binds an OAuth sign-in attempt to the browser that started it.
pub const SIGNIN_COOKIE: &str = "msync_signin";

fn now() -> i64 {
    Utc::now().timestamp()
}

/// `scheme://host[:port]` of a URL.
pub fn origin_of(url: &str) -> String {
    let url = url.trim();
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let host = rest.split(['/', '?', '#']).next().unwrap_or("");
            format!("{}://{}", scheme.to_ascii_lowercase(), host.to_ascii_lowercase())
        }
        None => String::new(),
    }
}

/// A state-changing cookie request must carry an `Origin` equal to `MSYNC_PUBLIC_URL`'s.
/// A missing `Origin` is refused (fail closed).
pub fn same_origin(req: &HttpRequest, public_url: &str) -> bool {
    if mutant::on("origin_unchecked") {
        return true;
    }
    let expected = origin_of(public_url);
    req.headers()
        .get("Origin")
        .and_then(|h| h.to_str().ok())
        .map_or(false, |o| !expected.is_empty() && o.trim().trim_end_matches('/').eq_ignore_ascii_case(&expected))
}

pub fn cross_origin_refused() -> HttpResponse {
    HttpResponse::Forbidden().json(json!({"status": "error", "error": "cross_origin", "message": "cross-origin request refused"}))
}

/// The session cookie as set on sign-in.
pub fn session_cookie(token: &str) -> Cookie<'static> {
    Cookie::build(SESSION_COOKIE, token.to_string())
        .path("/")
        .secure(true)
        .http_only(!mutant::on("cookie_not_httponly"))
        .same_site(SameSite::Lax)
        .max_age(CookieDuration::seconds(rules::SESSION_TTL_SECS))
        .finish()
}

fn expired_cookie(name: &'static str, path: &'static str) -> Cookie<'static> {
    Cookie::build(name, "")
        .path(path)
        .secure(true)
        .http_only(true)
        .same_site(SameSite::Lax)
        .max_age(CookieDuration::ZERO)
        .finish()
}

/// The browser session, from the cookie only.
pub async fn session(req: &HttpRequest, pool: &sqlx::SqlitePool) -> Option<(SessionInfo, String)> {
    let token = match req.cookie(SESSION_COOKIE) {
        Some(c) => c.value().to_string(),
        None if mutant::on("session_from_bearer") => crate::auth::tokens::bearer(req)?.to_string(),
        None => return None,
    };
    match store::session_for_token(pool, &token, now()).await {
        Ok(Some(s)) => Some((s, token)),
        Ok(None) => None,
        Err(e) => {
            error!("session lookup failed: {}", e);
            None
        }
    }
}

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

pub fn page(status: actix_web::http::StatusCode, title: &str, body_html: &str) -> HttpResponse {
    HttpResponse::build(status)
        .content_type("text/html; charset=utf-8")
        .insert_header(("Cache-Control", "no-store"))
        .insert_header(("X-Frame-Options", "DENY"))
        .insert_header(("Content-Security-Policy", "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'"))
        .body(format!(
            "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>{}</title>\
             <style>body{{font-family:system-ui,sans-serif;max-width:32rem;margin:3rem auto;padding:0 1rem;line-height:1.5}}button{{font-size:1rem;padding:.5rem 1rem}}</style>\
             </head><body><h1>{}</h1>{}</body></html>",
            html_escape(title),
            html_escape(title),
            body_html
        ))
}

fn client_ip(req: &HttpRequest) -> String {
    req.connection_info().realip_remote_addr().unwrap_or("unknown").to_string()
}

fn pkce_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn refused(status: actix_web::http::StatusCode, code: &str, message: &str) -> HttpResponse {
    HttpResponse::build(status).json(json!({"status": "error", "error": code, "message": message}))
}

// ---------------------------------------------------------------- OAuth sign-in

#[derive(Debug, Deserialize)]
pub struct StartQuery {
    pub next: Option<String>,
    pub link: Option<String>,
}

#[get("/auth/{provider}/start")]
pub async fn start(req: HttpRequest, path: web::Path<String>, q: web::Query<StartQuery>, ctx: web::Data<AccountsContext>) -> HttpResponse {
    use actix_web::http::StatusCode as S;
    let Some(provider) = Provider::parse(&path).filter(|p| *p != Provider::Email) else {
        return refused(S::NOT_FOUND, "unknown_provider", "unknown sign-in provider");
    };
    let Some(cfg) = ctx.providers.get(&provider) else {
        return refused(S::NOT_FOUND, "provider_disabled", "this sign-in provider is not enabled");
    };
    // An explicit link needs the signed-in session and that session's link nonce.
    let link_to = match &q.link {
        None => None,
        Some(nonce) => match session(&req, &ctx.pool).await {
            Some((s, token)) if rules::link_nonce(&token) == *nonce => Some(s.account_id),
            _ => return refused(S::FORBIDDEN, "link_requires_session", "linking needs a signed-in session"),
        },
    };
    let (browser, verifier) = match (rules::random_token(), rules::random_token()) {
        (Ok(b), Ok(v)) => (b, v),
        _ => return refused(S::INTERNAL_SERVER_ERROR, "rng", "could not start sign-in"),
    };
    let next = rules::safe_next(q.next.as_deref());
    let state = match store::create_login_state(&ctx.pool, provider, &browser, &verifier, link_to.as_deref(), Some(&next), now()).await {
        Ok(s) => s,
        Err(e) => {
            error!("sign-in start failed: {}", e);
            return refused(S::INTERNAL_SERVER_ERROR, "db", "could not start sign-in");
        }
    };
    let location = cfg.authorize_redirect(&ctx.redirect_uri(provider), &state, &pkce_challenge(&verifier));
    HttpResponse::Found()
        .insert_header(("Location", location))
        .insert_header(("Cache-Control", "no-store"))
        .cookie(
            Cookie::build(SIGNIN_COOKIE, browser)
                .path("/auth")
                .secure(true)
                .http_only(true)
                .same_site(SameSite::Lax)
                .max_age(CookieDuration::seconds(rules::LOGIN_STATE_TTL_SECS))
                .finish(),
        )
        .finish()
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

#[get("/auth/{provider}/callback")]
pub async fn callback(req: HttpRequest, path: web::Path<String>, q: web::Query<CallbackQuery>, ctx: web::Data<AccountsContext>) -> HttpResponse {
    use actix_web::http::StatusCode as S;
    let Some(provider) = Provider::parse(&path).filter(|p| *p != Provider::Email) else {
        return refused(S::NOT_FOUND, "unknown_provider", "unknown sign-in provider");
    };
    let Some(cfg) = ctx.providers.get(&provider) else {
        return refused(S::NOT_FOUND, "provider_disabled", "this sign-in provider is not enabled");
    };
    if q.error.is_some() && !mutant::on("ignore_provider_error") {
        warn!("{} sign-in: the provider returned an error", provider.label());
        return refused(S::BAD_REQUEST, "provider_error", "the provider did not complete the sign-in");
    }
    let (Some(code), Some(state)) = (q.code.as_deref(), q.state.as_deref()) else {
        return refused(S::BAD_REQUEST, "invalid_request", "missing code or state");
    };
    let Some(browser) = req.cookie(SIGNIN_COOKIE).map(|c| c.value().to_string()) else {
        return refused(S::BAD_REQUEST, "invalid_state", "this sign-in was not started in this browser");
    };
    let login_state = match store::take_login_state(&ctx.pool, provider, state, &browser, now()).await {
        Ok(Some(s)) => s,
        Ok(None) => return refused(S::BAD_REQUEST, "invalid_state", "unknown, used, expired or foreign sign-in state"),
        Err(e) => {
            error!("sign-in state lookup failed: {}", e);
            return refused(S::INTERNAL_SERVER_ERROR, "db", "sign-in failed");
        }
    };
    // A link must finish in the same signed-in account that started it.
    let current = session(&req, &ctx.pool).await;
    if let Some(link) = &login_state.link_account_id {
        if current.as_ref().map(|(s, _)| &s.account_id) != Some(link) {
            return refused(S::FORBIDDEN, "link_requires_session", "the account that started this link is not signed in here");
        }
    }
    let profile = match ctx.api.complete(cfg, code, &login_state.pkce_verifier, &ctx.redirect_uri(provider)).await {
        Ok(p) => p,
        Err(e) => {
            warn!("{} sign-in failed at the provider: {}", provider.label(), e);
            return refused(S::BAD_GATEWAY, "provider_error", "the provider did not complete the sign-in");
        }
    };
    finish_sign_in(&req, &ctx, &profile, login_state.link_account_id.as_deref(), login_state.next_path.as_deref(), true).await
}

async fn finish_sign_in(
    req: &HttpRequest,
    ctx: &AccountsContext,
    profile: &super::ProviderProfile,
    link_to: Option<&str>,
    next: Option<&str>,
    clear_signin_cookie: bool,
) -> HttpResponse {
    use actix_web::http::StatusCode as S;
    let t = now();
    let result = match store::complete_sign_in(&ctx.pool, profile, link_to, t).await {
        Ok(r) => r,
        Err(AccountsError::Refused(r)) => {
            return refused(S::CONFLICT, r.code(), "this identity cannot be linked to this account");
        }
        Err(AccountsError::NotActive) => return refused(S::FORBIDDEN, "account_not_active", "this account is suspended"),
        Err(e) => {
            error!("sign-in failed: {}", e);
            return refused(S::INTERNAL_SERVER_ERROR, "db", "sign-in failed");
        }
    };
    let mut resp = HttpResponse::Found();
    resp.insert_header(("Location", rules::safe_next(next))).insert_header(("Cache-Control", "no-store"));
    if clear_signin_cookie {
        resp.cookie(expired_cookie(SIGNIN_COOKIE, "/auth"));
    }
    if link_to.is_none() {
        let ua = req.headers().get("User-Agent").and_then(|h| h.to_str().ok());
        match store::create_session(&ctx.pool, &result.account_id, Some(&result.identity_id), Some(&client_ip(req)), ua, t).await {
            Ok(token) => {
                resp.cookie(session_cookie(&token));
            }
            Err(e) => {
                error!("session creation failed: {}", e);
                return refused(S::INTERNAL_SERVER_ERROR, "db", "sign-in failed");
            }
        }
    }
    resp.finish()
}

// ------------------------------------------------------------------- sessions

#[derive(Debug, Deserialize)]
pub struct LogoutQuery {
    #[serde(default)]
    pub all: bool,
}

#[post("/auth/logout")]
pub async fn logout(req: HttpRequest, q: web::Query<LogoutQuery>, ctx: web::Data<AccountsContext>) -> HttpResponse {
    if !same_origin(&req, &ctx.public_url) {
        return cross_origin_refused();
    }
    let Some((s, _)) = session(&req, &ctx.pool).await else {
        return HttpResponse::Unauthorized().cookie(expired_cookie(SESSION_COOKIE, "/")).json(json!({"status": "error", "error": "no_session"}));
    };
    let r = if q.all {
        store::revoke_all_sessions(&ctx.pool, &s.account_id, now()).await.map(|_| ())
    } else {
        store::revoke_session(&ctx.pool, &s.id, now()).await
    };
    if let Err(e) = r {
        error!("logout failed: {}", e);
        return refused(actix_web::http::StatusCode::INTERNAL_SERVER_ERROR, "db", "logout failed");
    }
    HttpResponse::Ok().cookie(expired_cookie(SESSION_COOKIE, "/")).json(json!({"status": "success", "all": q.all}))
}

#[get("/account")]
pub async fn get_account(req: HttpRequest, ctx: web::Data<AccountsContext>) -> HttpResponse {
    let Some((s, token)) = session(&req, &ctx.pool).await else {
        return HttpResponse::Unauthorized().json(json!({"status": "error", "error": "no_session", "message": "sign in first"}));
    };
    let pool = &ctx.pool;
    let (account, identities, events) = match (
        store::account(pool, &s.account_id).await,
        store::identities_of(pool, &s.account_id, s.identity_id.as_deref()).await,
        store::events_of(pool, &s.account_id).await,
    ) {
        (Ok(Some(a)), Ok(i), Ok(e)) => (a, i, e),
        _ => return refused(actix_web::http::StatusCode::INTERNAL_SERVER_ERROR, "db", "could not load the account"),
    };
    let (member, standing) = match ctx.cra.active() {
        Some(active) => match store::member_for_account(pool, &active.snapshot, &s.account_id).await {
            Ok(Some(m)) => {
                let st = crate::cra::standing(&active.snapshot, &m, ctx.cra.merit_of(&m), ctx.today());
                (Some(m), st)
            }
            Ok(None) => (None, None),
            Err(e) => {
                error!("member lookup failed: {}", e);
                (None, None)
            }
        },
        None => (None, None),
    };
    let mut enabled: Vec<&str> = ctx.providers.keys().map(|p| p.as_str()).collect();
    enabled.sort();
    HttpResponse::Ok().insert_header(("Cache-Control", "no-store")).json(json!({
        "account": account,
        "identities": identities,
        "events": events,
        "member_id": member,
        "standing": standing,
        "link_nonce": rules::link_nonce(&token),
        "providers": enabled,
    }))
}

#[post("/account/identities/{id}/unlink")]
pub async fn unlink(req: HttpRequest, path: web::Path<String>, ctx: web::Data<AccountsContext>) -> HttpResponse {
    use actix_web::http::StatusCode as S;
    if !same_origin(&req, &ctx.public_url) {
        return cross_origin_refused();
    }
    let Some((s, _)) = session(&req, &ctx.pool).await else {
        return HttpResponse::Unauthorized().json(json!({"status": "error", "error": "no_session"}));
    };
    match store::unlink_identity(&ctx.pool, &s.account_id, &path, now()).await {
        Ok(()) => HttpResponse::Ok().json(json!({"status": "success", "unlinked": path.into_inner()})),
        Err(UnlinkError::Refused(r)) => refused(
            if r == rules::UnlinkRefusal::NotYours { S::NOT_FOUND } else { S::CONFLICT },
            r.code(),
            match r {
                rules::UnlinkRefusal::NotYours => "not one of your identities",
                rules::UnlinkRefusal::LastOne => "this is your last way to sign in; link another first",
            },
        ),
        Err(UnlinkError::Db(e)) => {
            error!("unlink failed: {}", e);
            refused(S::INTERNAL_SERVER_ERROR, "db", "unlink failed")
        }
    }
}

// ----------------------------------------------------------------- email links

#[derive(Debug, Deserialize)]
pub struct EmailStartBody {
    pub email: String,
    /// The signed-in session's link nonce, to add this address to that account.
    pub link: Option<String>,
}

#[post("/auth/email/start")]
pub async fn email_start(req: HttpRequest, body: web::Json<EmailStartBody>, ctx: web::Data<AccountsContext>) -> HttpResponse {
    use actix_web::http::StatusCode as S;
    if !same_origin(&req, &ctx.public_url) {
        return cross_origin_refused();
    }
    let link_to = match &body.link {
        None => None,
        Some(nonce) => match session(&req, &ctx.pool).await {
            Some((s, token)) if rules::link_nonce(&token) == *nonce => Some(s.account_id),
            _ => return refused(S::FORBIDDEN, "link_requires_session", "linking needs a signed-in session"),
        },
    };
    match store::create_email_token(&ctx.pool, &body.email, &client_ip(&req), link_to.as_deref(), now()).await {
        Ok(EmailRequest::Sent { token, email }) => {
            let url = format!("{}/auth/email/verify?token={}", ctx.public_url, token);
            if let Err(e) = ctx.mailer.send_sign_in_link(&email, &url).await {
                error!("sending the sign-in link to {} failed: {}", rules::mask_email(&email), e);
            }
            HttpResponse::Ok().json(json!({"status": "success", "message": "if this address can sign in, a link is on its way; it works once, for 15 minutes"}))
        }
        Ok(EmailRequest::Invalid) => refused(S::BAD_REQUEST, "invalid_email", "that is not an email address"),
        Ok(EmailRequest::Limited(_)) => refused(S::TOO_MANY_REQUESTS, "rate_limited", "too many sign-in links requested; try again later"),
        Err(e) => {
            error!("email link failed: {}", e);
            refused(S::INTERNAL_SERVER_ERROR, "db", "could not create a sign-in link")
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct TokenQuery {
    pub token: String,
}

#[get("/auth/email/verify")]
pub async fn email_verify_page(q: web::Query<TokenQuery>, ctx: web::Data<AccountsContext>) -> HttpResponse {
    use actix_web::http::StatusCode as S;
    match store::email_token_state(&ctx.pool, &q.token, now()).await {
        Ok(Some((rules::TokenState::Valid, email))) => page(
            S::OK,
            "Sign in to MSync",
            &format!(
                "<p>Sign in as {}?</p><form method=\"post\" action=\"/auth/email/verify\"><input type=\"hidden\" name=\"token\" value=\"{}\"><button type=\"submit\">Sign in</button></form>",
                html_escape(&rules::mask_email(&email)),
                html_escape(&q.token)
            ),
        ),
        Ok(Some((rules::TokenState::Used, _))) => page(S::GONE, "Link already used", "<p>This sign-in link was already used. Request a new one.</p>"),
        Ok(Some((rules::TokenState::Expired, _))) => page(S::GONE, "Link expired", "<p>This sign-in link has expired. Request a new one.</p>"),
        Ok(None) => page(S::NOT_FOUND, "Unknown link", "<p>This sign-in link is not valid.</p>"),
        Err(_) => page(S::INTERNAL_SERVER_ERROR, "Sign-in failed", "<p>Try again later.</p>"),
    }
}

#[derive(Debug, Deserialize)]
pub struct TokenForm {
    pub token: String,
}

#[post("/auth/email/verify")]
pub async fn email_verify(req: HttpRequest, form: web::Form<TokenForm>, ctx: web::Data<AccountsContext>) -> HttpResponse {
    use actix_web::http::StatusCode as S;
    if !same_origin(&req, &ctx.public_url) {
        return cross_origin_refused();
    }
    match store::consume_email_token(&ctx.pool, &form.token, now()).await {
        Ok(Some((email, link_to))) => finish_sign_in(&req, &ctx, &store::email_profile(&email), link_to.as_deref(), None, false).await,
        Ok(None) => page(S::GONE, "Link not valid", "<p>This sign-in link is unknown, used or expired. Request a new one.</p>"),
        Err(e) => {
            error!("email link failed: {}", e);
            page(S::INTERNAL_SERVER_ERROR, "Sign-in failed", "<p>Try again later.</p>")
        }
    }
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(start)
        .service(callback)
        .service(logout)
        .service(email_start)
        .service(email_verify_page)
        .service(email_verify)
        .service(get_account)
        .service(unlink);
}
