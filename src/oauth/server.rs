//! The authorization server endpoints.
//!
//! Redirects go only to a registered, exactly matching redirect URI. An unknown client or a
//! redirect URI that is not registered gets an error page and never a redirect.

use actix_web::http::StatusCode;
use actix_web::{get, post, web, HttpRequest, HttpResponse};
use base64::Engine;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tracing::{error, warn};

use super::clients::{self, Client};
use super::{active_member, jwt, keys, member_claims, revoke_grant, OAuthContext};
use super::{ACCESS_TTL_SECS, CODE_TTL_SECS, ID_TTL_SECS, REFRESH_TTL_SECS, SCOPES};
use crate::accounts::mutant;
use crate::accounts::routes::{cross_origin_refused, html_escape, page, same_origin, session};
use crate::accounts::rules::{consent_nonce, random_id, random_token, token_hash};

const B64URL: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

fn now() -> i64 {
    Utc::now().timestamp()
}

fn disabled() -> HttpResponse {
    HttpResponse::ServiceUnavailable().json(json!({
        "error": "temporarily_unavailable",
        "error_description": "the MSync OAuth provider is not configured (MSYNC_KEY_ENCRYPTION_KEY)"
    }))
}

fn split_scopes(s: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for x in s.unwrap_or("").split(' ').filter(|x| !x.is_empty()) {
        if !out.iter().any(|o| o == x) {
            out.push(x.to_string());
        }
    }
    out
}

/// `redirect_uri` plus query parameters.
fn redirect_with(uri: &str, params: &[(&str, &str)]) -> HttpResponse {
    let sep = if uri.contains('?') { '&' } else { '?' };
    let q = crate::accounts::providers::encode_query(params);
    HttpResponse::Found().insert_header(("Location", format!("{uri}{sep}{q}"))).insert_header(("Cache-Control", "no-store")).finish()
}

/// PKCE S256: `BASE64URL(SHA256(verifier)) == challenge`, verifier 43..=128 unreserved characters.
pub fn pkce_ok(verifier: Option<&str>, challenge: &str) -> bool {
    if mutant::on("pkce_skip") {
        return true;
    }
    let Some(v) = verifier else { return false };
    if !(43..=128).contains(&v.len()) || !v.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')) {
        return false;
    }
    let computed = B64URL.encode(Sha256::digest(v.as_bytes()));
    bool::from(computed.as_bytes().ct_eq(challenge.as_bytes()))
}

// ------------------------------------------------------------------ discovery

#[get("/.well-known/openid-configuration")]
pub async fn discovery(ctx: web::Data<OAuthContext>) -> HttpResponse {
    if ctx.kek.is_none() {
        return disabled();
    }
    let i = &ctx.issuer;
    HttpResponse::Ok().json(json!({
        "issuer": i,
        "authorization_endpoint": format!("{i}/oauth/authorize"),
        "token_endpoint": format!("{i}/oauth/token"),
        "userinfo_endpoint": format!("{i}/oauth/userinfo"),
        "jwks_uri": format!("{i}/oauth/jwks"),
        "revocation_endpoint": format!("{i}/oauth/revoke"),
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "subject_types_supported": ["pairwise"],
        "id_token_signing_alg_values_supported": [keys::ALG],
        "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post", "none"],
        "revocation_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post", "none"],
        "code_challenge_methods_supported": ["S256"],
        "scopes_supported": SCOPES,
        "claims_supported": ["iss", "sub", "aud", "exp", "iat", "auth_time", "nonce", "azp", "name", "handle", "groups", "titles", "rank", "merit"],
        "authorization_response_iss_parameter_supported": true,
    }))
}

#[get("/oauth/jwks")]
pub async fn jwks(ctx: web::Data<OAuthContext>) -> HttpResponse {
    let Some(kek) = &ctx.kek else { return disabled() };
    // Make sure a key exists before anyone asks for one.
    if let Err(e) = keys::current(&ctx.pool, kek, now()).await {
        error!("signing key unavailable: {}", e);
        return HttpResponse::ServiceUnavailable().json(json!({"error": "temporarily_unavailable"}));
    }
    match keys::jwks(&ctx.pool, now()).await {
        Ok(v) => HttpResponse::Ok().insert_header(("Cache-Control", "public, max-age=300")).json(v),
        Err(e) => {
            error!("jwks failed: {}", e);
            HttpResponse::ServiceUnavailable().json(json!({"error": "temporarily_unavailable"}))
        }
    }
}

// ------------------------------------------------------------------ authorize

#[derive(Debug, Clone, Deserialize)]
pub struct AuthorizeParams {
    pub response_type: Option<String>,
    pub client_id: Option<String>,
    pub redirect_uri: Option<String>,
    pub scope: Option<String>,
    pub state: Option<String>,
    pub nonce: Option<String>,
    pub code_challenge: Option<String>,
    pub code_challenge_method: Option<String>,
    /// Consent form only.
    pub csrf: Option<String>,
    /// Consent form only: `allow` or `deny`.
    pub decision: Option<String>,
}

/// A request that passed validation: the redirect URI is the client's.
struct Valid {
    client: Client,
    redirect_uri: String,
    scopes: Vec<String>,
    state: Option<String>,
    nonce: Option<String>,
    challenge: String,
}

#[allow(clippy::large_enum_variant)]
enum Checked {
    Ok(Valid),
    /// No redirect: unknown client or an unregistered redirect URI.
    Page(HttpResponse),
    /// Redirect an error to the (registered) redirect URI.
    Redirect(HttpResponse),
}

async fn check(ctx: &OAuthContext, p: &AuthorizeParams) -> Checked {
    let bad = |msg: &str| Checked::Page(page(StatusCode::BAD_REQUEST, "Sign-in request refused", &format!("<p>{}</p>", html_escape(msg))));
    let Some(client_id) = p.client_id.as_deref() else { return bad("The request names no client.") };
    let client = match clients::get(&ctx.pool, client_id).await {
        Ok(Some(c)) => c,
        Ok(None) => return bad("This platform is not registered with MSync."),
        Err(e) => {
            error!("client lookup failed: {}", e);
            return Checked::Page(page(StatusCode::INTERNAL_SERVER_ERROR, "Sign-in failed", "<p>Try again later.</p>"));
        }
    };
    let Some(redirect_uri) = p.redirect_uri.clone().filter(|u| client.redirect_allowed(u)) else {
        return bad("The redirect address is not one this platform registered.");
    };
    let state = p.state.clone();
    let err = |code: &str, desc: &str| {
        let mut q = vec![("error", code), ("error_description", desc), ("iss", ctx.issuer.as_str())];
        if let Some(s) = state.as_deref() {
            q.push(("state", s));
        }
        Checked::Redirect(redirect_with(&redirect_uri, &q))
    };
    if p.response_type.as_deref() != Some("code") {
        return err("unsupported_response_type", "only response_type=code is supported");
    }
    let challenge = match (p.code_challenge.as_deref(), p.code_challenge_method.as_deref()) {
        (Some(c), Some("S256")) if (43..=128).contains(&c.len()) => c.to_string(),
        _ => return err("invalid_request", "PKCE is required: code_challenge with code_challenge_method=S256"),
    };
    let scopes = split_scopes(p.scope.as_deref());
    if !scopes.iter().any(|s| s == "openid") {
        return err("invalid_scope", "the openid scope is required");
    }
    if scopes.iter().any(|s| !SCOPES.contains(&s.as_str()) || !client.allowed_scopes.contains(s)) {
        return err("invalid_scope", "a requested scope is unknown or not allowed for this client");
    }
    if p.nonce.as_deref().is_some_and(|n| n.len() > 512) || state.as_deref().is_some_and(|s| s.len() > 2048) {
        return err("invalid_request", "nonce or state too long");
    }
    Checked::Ok(Valid { client, redirect_uri, scopes, state, nonce: p.nonce.clone(), challenge })
}

fn hidden(name: &str, v: Option<&str>) -> String {
    v.map(|v| format!("<input type=\"hidden\" name=\"{}\" value=\"{}\">", name, html_escape(v))).unwrap_or_default()
}

fn sign_in_first(ctx: &OAuthContext, req: &HttpRequest) -> HttpResponse {
    let next = format!("{}?{}", req.path(), req.query_string());
    let links: String = ctx
        .sign_in_providers
        .iter()
        .map(|p| {
            format!(
                "<li><a href=\"/auth/{}/start?next={}\">Sign in with {}</a></li>",
                p.as_str(),
                html_escape(&crate::accounts::providers::pct(&next)),
                p.label()
            )
        })
        .collect();
    page(StatusCode::UNAUTHORIZED, "Sign in to MSync", &format!("<p>Sign in to continue.</p><ul>{links}</ul>"))
}

async fn issue_code(ctx: &OAuthContext, v: &Valid, account_id: &str) -> HttpResponse {
    let code = match random_token() {
        Ok(c) => c,
        Err(_) => return page(StatusCode::INTERNAL_SERVER_ERROR, "Sign-in failed", "<p>Try again later.</p>"),
    };
    let t = now();
    let r = sqlx::query(
        "INSERT INTO oauth_codes (code_hash, client_id, account_id, redirect_uri, scopes, nonce, code_challenge, auth_time, created_at, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(token_hash(&code))
    .bind(&v.client.client_id)
    .bind(account_id)
    .bind(&v.redirect_uri)
    .bind(serde_json::to_string(&v.scopes).unwrap_or_default())
    .bind(&v.nonce)
    .bind(&v.challenge)
    .bind(t)
    .bind(t)
    .bind(t + CODE_TTL_SECS)
    .execute(&ctx.pool)
    .await;
    if let Err(e) = r {
        error!("code insert failed: {}", e);
        return page(StatusCode::INTERNAL_SERVER_ERROR, "Sign-in failed", "<p>Try again later.</p>");
    }
    let mut q = vec![("code", code.as_str()), ("iss", ctx.issuer.as_str())];
    if let Some(s) = v.state.as_deref() {
        q.push(("state", s));
    }
    redirect_with(&v.redirect_uri, &q)
}

async fn consented(ctx: &OAuthContext, account_id: &str, client_id: &str, scopes: &[String]) -> bool {
    let row: Option<String> = sqlx::query_scalar("SELECT scopes FROM oauth_consents WHERE account_id = ? AND client_id = ?")
        .bind(account_id)
        .bind(client_id)
        .fetch_optional(&ctx.pool)
        .await
        .unwrap_or(None);
    let Some(row) = row else { return false };
    let granted: Vec<String> = serde_json::from_str(&row).unwrap_or_default();
    scopes.iter().all(|s| granted.contains(s))
}

/// Shared by GET and POST: validate, require the session, require an active member.
async fn authorize_common(req: &HttpRequest, ctx: &OAuthContext, p: &AuthorizeParams) -> Result<(Valid, String, String), HttpResponse> {
    let v = match check(ctx, p).await {
        Checked::Ok(v) => v,
        Checked::Page(r) | Checked::Redirect(r) => return Err(r),
    };
    // The browser session only. A bearer token (an API token) never authorizes this route.
    let Some((s, session_token)) = session(req, &ctx.pool).await else { return Err(sign_in_first(ctx, req)) };
    match active_member(&ctx.pool, &ctx.cra, &s.account_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            let mut q = vec![
                ("error", "access_denied"),
                ("error_description", "not an active member of this organization"),
                ("iss", ctx.issuer.as_str()),
            ];
            if let Some(st) = v.state.as_deref() {
                q.push(("state", st));
            }
            return Err(redirect_with(&v.redirect_uri, &q));
        }
        Err(e) => {
            error!("member lookup failed: {}", e);
            return Err(page(StatusCode::INTERNAL_SERVER_ERROR, "Sign-in failed", "<p>Try again later.</p>"));
        }
    }
    Ok((v, s.account_id, session_token))
}

#[get("/oauth/authorize")]
pub async fn authorize(req: HttpRequest, q: web::Query<AuthorizeParams>, ctx: web::Data<OAuthContext>) -> HttpResponse {
    if ctx.kek.is_none() {
        return disabled();
    }
    let (v, account, session_token) = match authorize_common(&req, &ctx, &q).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if consented(&ctx, &account, &v.client.client_id, &v.scopes).await {
        return issue_code(&ctx, &v, &account).await;
    }
    let scopes: String = v.scopes.iter().map(|s| format!("<li><code>{}</code></li>", html_escape(s))).collect();
    let form = format!(
        "<p><strong>{}</strong> asks to sign you in with MSync and read:</p><ul>{}</ul>\
         <form method=\"post\" action=\"/oauth/authorize\">{}{}{}{}{}{}{}{}{}\
         <button type=\"submit\" name=\"decision\" value=\"allow\">Allow</button> \
         <button type=\"submit\" name=\"decision\" value=\"deny\">Deny</button></form>",
        html_escape(&v.client.name),
        scopes,
        hidden("response_type", q.response_type.as_deref()),
        hidden("client_id", q.client_id.as_deref()),
        hidden("redirect_uri", q.redirect_uri.as_deref()),
        hidden("scope", q.scope.as_deref()),
        hidden("state", q.state.as_deref()),
        hidden("nonce", q.nonce.as_deref()),
        hidden("code_challenge", q.code_challenge.as_deref()),
        hidden("code_challenge_method", q.code_challenge_method.as_deref()),
        hidden("csrf", Some(&consent_nonce(&session_token))),
    );
    page(StatusCode::OK, "Allow access?", &form)
}

#[post("/oauth/authorize")]
pub async fn authorize_consent(req: HttpRequest, f: web::Form<AuthorizeParams>, ctx: web::Data<OAuthContext>) -> HttpResponse {
    if ctx.kek.is_none() {
        return disabled();
    }
    if !same_origin(&req, &ctx.issuer) {
        return cross_origin_refused();
    }
    let (v, account, session_token) = match authorize_common(&req, &ctx, &f).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if f.csrf.as_deref() != Some(consent_nonce(&session_token).as_str()) {
        return page(StatusCode::FORBIDDEN, "Sign-in request refused", "<p>The consent form expired. Start again.</p>");
    }
    if f.decision.as_deref() != Some("allow") {
        let mut q = vec![("error", "access_denied"), ("error_description", "the member declined"), ("iss", ctx.issuer.as_str())];
        if let Some(s) = v.state.as_deref() {
            q.push(("state", s));
        }
        return redirect_with(&v.redirect_uri, &q);
    }
    // Remember the consent: the union of what was granted before and now.
    let prev: Option<String> = sqlx::query_scalar("SELECT scopes FROM oauth_consents WHERE account_id = ? AND client_id = ?")
        .bind(&account)
        .bind(&v.client.client_id)
        .fetch_optional(&ctx.pool)
        .await
        .unwrap_or(None);
    let mut all: Vec<String> = prev.and_then(|p| serde_json::from_str(&p).ok()).unwrap_or_default();
    for s in &v.scopes {
        if !all.contains(s) {
            all.push(s.clone());
        }
    }
    let r = sqlx::query(
        "INSERT INTO oauth_consents (account_id, client_id, scopes, granted_at) VALUES (?, ?, ?, ?)
         ON CONFLICT (account_id, client_id) DO UPDATE SET scopes = excluded.scopes, granted_at = excluded.granted_at",
    )
    .bind(&account)
    .bind(&v.client.client_id)
    .bind(serde_json::to_string(&all).unwrap_or_default())
    .bind(now())
    .execute(&ctx.pool)
    .await;
    if let Err(e) = r {
        error!("consent insert failed: {}", e);
    }
    issue_code(&ctx, &v, &account).await
}

// ---------------------------------------------------------------------- token

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenError {
    pub status: StatusCode,
    pub code: &'static str,
    pub description: &'static str,
}

fn terr(code: &'static str, description: &'static str) -> TokenError {
    let status = if code == "invalid_client" { StatusCode::UNAUTHORIZED } else { StatusCode::BAD_REQUEST };
    TokenError { status, code, description }
}

impl TokenError {
    pub fn response(&self) -> HttpResponse {
        let mut r = HttpResponse::build(self.status);
        r.insert_header(("Cache-Control", "no-store")).insert_header(("Pragma", "no-cache"));
        if self.status == StatusCode::UNAUTHORIZED {
            r.insert_header(("WWW-Authenticate", "Basic realm=\"msync\""));
        }
        r.json(json!({"error": self.code, "error_description": self.description}))
    }
}

fn server_error(e: impl std::fmt::Display) -> TokenError {
    error!("token endpoint: {}", e);
    TokenError { status: StatusCode::INTERNAL_SERVER_ERROR, code: "server_error", description: "try again later" }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TokenForm {
    pub grant_type: Option<String>,
    pub code: Option<String>,
    pub redirect_uri: Option<String>,
    pub code_verifier: Option<String>,
    pub refresh_token: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub token: Option<String>,
    pub token_type_hint: Option<String>,
}

/// The client from `Authorization: Basic` or the form, authenticated.
async fn client_auth(req: &HttpRequest, ctx: &OAuthContext, f: &TokenForm) -> Result<Client, TokenError> {
    let basic = req
        .headers()
        .get("Authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Basic "))
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b.trim()).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| s.split_once(':').map(|(a, b)| (a.to_string(), b.to_string())));
    let (id, secret) = match (basic, &f.client_id, &f.client_secret) {
        (Some(_), _, Some(_)) => return Err(terr("invalid_request", "use one client authentication method")),
        (Some((id, s)), form_id, None) => {
            if form_id.as_deref().is_some_and(|f| f != id) {
                return Err(terr("invalid_client", "client mismatch"));
            }
            (id, Some(s))
        }
        (None, Some(id), s) => (id.clone(), s.clone()),
        (None, None, _) => return Err(terr("invalid_client", "client authentication required")),
    };
    let client = clients::get(&ctx.pool, &id).await.map_err(server_error)?.ok_or(terr("invalid_client", "unknown client"))?;
    if !client.authenticate(secret.as_deref().filter(|s| !s.is_empty())) {
        return Err(terr("invalid_client", "client authentication failed"));
    }
    Ok(client)
}

#[derive(Debug, Clone)]
struct GrantRow {
    id: String,
    account_id: String,
    scopes: Vec<String>,
    auth_time: i64,
}

/// Issue an access token, a refresh token and (with `openid`) an ID token for a grant. The
/// account must be an active member; otherwise the family is revoked.
async fn issue(ctx: &OAuthContext, client: &Client, g: &GrantRow, nonce: Option<&str>, t: i64) -> Result<Value, TokenError> {
    let kek = ctx.kek.as_ref().ok_or(terr("temporarily_unavailable", "provider not configured"))?;
    let Some(member) = active_member(&ctx.pool, &ctx.cra, &g.account_id).await.map_err(server_error)? else {
        revoke_grant(&ctx.pool, &g.id, "not an active member", t).await.map_err(server_error)?;
        return Err(terr("invalid_grant", "not an active member of this organization"));
    };
    let key = keys::current(&ctx.pool, kek, t).await.map_err(server_error)?;
    let sub = client.pairwise_sub(&g.account_id);
    let jti = random_token().map_err(server_error)?;
    let access = jwt::sign(
        &key,
        "at+jwt",
        &json!({
            "iss": ctx.issuer, "sub": sub, "aud": client.client_id, "client_id": client.client_id,
            "iat": t, "exp": t + ACCESS_TTL_SECS, "jti": jti, "scope": g.scopes.join(" "), "gid": g.id,
        }),
    )
    .map_err(server_error)?;
    let refresh = format!("msr_{}", random_token().map_err(server_error)?);
    sqlx::query("INSERT INTO oauth_refresh_tokens (token_hash, grant_id, created_at, expires_at) VALUES (?, ?, ?, ?)")
        .bind(token_hash(&refresh))
        .bind(&g.id)
        .bind(t)
        .bind(t + REFRESH_TTL_SECS)
        .execute(&ctx.pool)
        .await
        .map_err(server_error)?;
    let mut out = json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": ACCESS_TTL_SECS,
        "refresh_token": refresh,
        "scope": g.scopes.join(" "),
    });
    if g.scopes.iter().any(|s| s == "openid") {
        let mut claims = member_claims(ctx, client, &member, &g.scopes);
        claims.insert("iss".into(), json!(ctx.issuer));
        claims.insert("sub".into(), json!(sub));
        claims.insert("aud".into(), json!(client.client_id));
        claims.insert("azp".into(), json!(client.client_id));
        claims.insert("iat".into(), json!(t));
        claims.insert("exp".into(), json!(t + ID_TTL_SECS));
        claims.insert("auth_time".into(), json!(g.auth_time));
        if let Some(n) = nonce.filter(|_| !mutant::on("nonce_dropped")) {
            claims.insert("nonce".into(), json!(n));
        }
        out["id_token"] = json!(jwt::sign(&key, "JWT", &Value::Object(claims)).map_err(server_error)?);
    }
    Ok(out)
}

type CodeRow = (String, String, String, String, Option<String>, String, i64, i64, Option<i64>, Option<String>);

/// `grant_type=authorization_code`.
pub async fn exchange_code(ctx: &OAuthContext, client: &Client, f: &TokenForm, t: i64) -> Result<Value, TokenError> {
    let code = f.code.as_deref().ok_or(terr("invalid_request", "code is required"))?;
    let hash = token_hash(code);
    let row: Option<CodeRow> = sqlx::query_as(
        "SELECT client_id, account_id, redirect_uri, scopes, nonce, code_challenge, auth_time, expires_at, used_at, grant_id
         FROM oauth_codes WHERE code_hash = ?",
    )
    .bind(&hash)
    .fetch_optional(&ctx.pool)
    .await
    .map_err(server_error)?;
    let Some((code_client, account_id, redirect_uri, scopes, nonce, challenge, auth_time, _exp, _used, _)) = row else {
        return Err(terr("invalid_grant", "unknown code"));
    };
    if code_client != client.client_id && !mutant::on("code_any_client") {
        return Err(terr("invalid_grant", "the code was issued to another client"));
    }
    // Single use, in the same statement that checks it.
    let consume = if mutant::on("code_reusable") {
        "UPDATE oauth_codes SET used_at = ? WHERE code_hash = ? AND expires_at > ?"
    } else {
        "UPDATE oauth_codes SET used_at = ? WHERE code_hash = ? AND used_at IS NULL AND expires_at > ?"
    };
    let n = sqlx::query(consume).bind(t).bind(&hash).bind(t).execute(&ctx.pool).await.map_err(server_error)?.rows_affected();
    if n == 0 {
        let (used, grant): (Option<i64>, Option<String>) =
            sqlx::query_as("SELECT used_at, grant_id FROM oauth_codes WHERE code_hash = ?").bind(&hash).fetch_one(&ctx.pool).await.map_err(server_error)?;
        if used.is_some() {
            // A replayed code: everything issued from it is revoked.
            if let Some(g) = grant {
                warn!("authorization code replayed; revoking token family {}", g);
                revoke_grant(&ctx.pool, &g, "authorization code replayed", t).await.map_err(server_error)?;
            }
            return Err(terr("invalid_grant", "the code was already used"));
        }
        return Err(terr("invalid_grant", "the code expired"));
    }
    if f.redirect_uri.as_deref() != Some(redirect_uri.as_str()) {
        return Err(terr("invalid_grant", "redirect_uri does not match the authorization request"));
    }
    if !pkce_ok(f.code_verifier.as_deref(), &challenge) {
        return Err(terr("invalid_grant", "PKCE verification failed"));
    }
    let gid = random_id("grt_").map_err(server_error)?;
    sqlx::query("INSERT INTO oauth_grants (id, client_id, account_id, scopes, auth_time, created_at) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(&gid)
        .bind(&client.client_id)
        .bind(&account_id)
        .bind(&scopes)
        .bind(auth_time)
        .bind(t)
        .execute(&ctx.pool)
        .await
        .map_err(server_error)?;
    sqlx::query("UPDATE oauth_codes SET grant_id = ? WHERE code_hash = ?").bind(&gid).bind(&hash).execute(&ctx.pool).await.map_err(server_error)?;
    let g = GrantRow { id: gid, account_id, scopes: serde_json::from_str(&scopes).unwrap_or_default(), auth_time };
    issue(ctx, client, &g, nonce.as_deref(), t).await
}

type RefreshRow = (String, Option<i64>, Option<i64>, i64, String, String, String, i64, Option<i64>);

/// `grant_type=refresh_token`: rotate. Presenting a used refresh token revokes the family.
pub async fn refresh(ctx: &OAuthContext, client: &Client, f: &TokenForm, t: i64) -> Result<Value, TokenError> {
    let rt = f.refresh_token.as_deref().ok_or(terr("invalid_request", "refresh_token is required"))?;
    let hash = token_hash(rt);
    let row: Option<RefreshRow> = sqlx::query_as(
        "SELECT r.grant_id, r.used_at, r.revoked_at, r.expires_at, g.client_id, g.account_id, g.scopes, g.auth_time, g.revoked_at
         FROM oauth_refresh_tokens r JOIN oauth_grants g ON g.id = r.grant_id WHERE r.token_hash = ?",
    )
    .bind(&hash)
    .fetch_optional(&ctx.pool)
    .await
    .map_err(server_error)?;
    let Some((gid, used, _revoked, _exp, g_client, account_id, scopes, auth_time, g_revoked)) = row else {
        return Err(terr("invalid_grant", "unknown refresh token"));
    };
    if g_client != client.client_id {
        return Err(terr("invalid_grant", "the refresh token belongs to another client"));
    }
    let consume = if mutant::on("refresh_reusable") {
        "UPDATE oauth_refresh_tokens SET used_at = ? WHERE token_hash = ? AND revoked_at IS NULL AND expires_at > ?"
    } else {
        "UPDATE oauth_refresh_tokens SET used_at = ? WHERE token_hash = ? AND used_at IS NULL AND revoked_at IS NULL AND expires_at > ?"
    };
    let n = sqlx::query(consume).bind(t).bind(&hash).bind(t).execute(&ctx.pool).await.map_err(server_error)?.rows_affected();
    if n == 0 {
        if used.is_some() && !mutant::on("refresh_reuse_no_revoke") {
            warn!("refresh token reused; revoking token family {}", gid);
            revoke_grant(&ctx.pool, &gid, "refresh token reused", t).await.map_err(server_error)?;
        }
        return Err(terr("invalid_grant", "the refresh token is used, revoked or expired"));
    }
    if g_revoked.is_some() {
        return Err(terr("invalid_grant", "the grant was revoked"));
    }
    let g = GrantRow { id: gid, account_id, scopes: serde_json::from_str(&scopes).unwrap_or_default(), auth_time };
    issue(ctx, client, &g, None, t).await
}

#[post("/oauth/token")]
pub async fn token(req: HttpRequest, f: web::Form<TokenForm>, ctx: web::Data<OAuthContext>) -> HttpResponse {
    if ctx.kek.is_none() {
        return disabled();
    }
    let client = match client_auth(&req, &ctx, &f).await {
        Ok(c) => c,
        Err(e) => return e.response(),
    };
    let t = now();
    let r = match f.grant_type.as_deref() {
        Some("authorization_code") => exchange_code(&ctx, &client, &f, t).await,
        Some("refresh_token") => refresh(&ctx, &client, &f, t).await,
        _ => Err(terr("unsupported_grant_type", "use authorization_code or refresh_token")),
    };
    match r {
        Ok(v) => HttpResponse::Ok().insert_header(("Cache-Control", "no-store")).insert_header(("Pragma", "no-cache")).json(v),
        Err(e) => e.response(),
    }
}

// -------------------------------------------------------------------- revoke

/// RFC 7009. Always 200 for an authenticated client, whether or not the token was known.
#[post("/oauth/revoke")]
pub async fn revoke(req: HttpRequest, f: web::Form<TokenForm>, ctx: web::Data<OAuthContext>) -> HttpResponse {
    if ctx.kek.is_none() {
        return disabled();
    }
    let client = match client_auth(&req, &ctx, &f).await {
        Ok(c) => c,
        Err(e) => return e.response(),
    };
    let Some(tok) = f.token.as_deref() else { return terr("invalid_request", "token is required").response() };
    let t = now();
    let gid: Option<String> = if tok.starts_with("msr_") {
        sqlx::query_scalar(
            "SELECT r.grant_id FROM oauth_refresh_tokens r JOIN oauth_grants g ON g.id = r.grant_id WHERE r.token_hash = ? AND g.client_id = ?",
        )
        .bind(token_hash(tok))
        .bind(&client.client_id)
        .fetch_optional(&ctx.pool)
        .await
        .unwrap_or(None)
    } else {
        let keys = keys::published(&ctx.pool, t).await.unwrap_or_default();
        jwt::verify_claims(tok, &keys, "at+jwt", &ctx.issuer, Some(&client.client_id), t)
            .ok()
            .and_then(|c| c.get("gid").and_then(Value::as_str).map(str::to_string))
    };
    if let Some(g) = gid {
        if let Err(e) = revoke_grant(&ctx.pool, &g, "revoked by the client", t).await {
            return server_error(e).response();
        }
    }
    HttpResponse::Ok().insert_header(("Cache-Control", "no-store")).finish()
}

// ------------------------------------------------------------------ userinfo

fn bearer_error(desc: &str) -> HttpResponse {
    HttpResponse::Unauthorized()
        .insert_header(("WWW-Authenticate", format!("Bearer error=\"invalid_token\", error_description=\"{desc}\"")))
        .json(json!({"error": "invalid_token", "error_description": desc}))
}

async fn userinfo_impl(req: HttpRequest, ctx: web::Data<OAuthContext>) -> HttpResponse {
    if ctx.kek.is_none() {
        return disabled();
    }
    let Some(tok) = crate::auth::tokens::bearer(&req) else { return bearer_error("missing bearer token") };
    let t = now();
    let keys = match keys::published(&ctx.pool, t).await {
        Ok(k) => k,
        Err(e) => return server_error(e).response(),
    };
    let claims = match jwt::verify_claims(tok, &keys, "at+jwt", &ctx.issuer, None, t) {
        Ok(c) => c,
        Err(e) => return bearer_error(&e.to_string()),
    };
    let (Some(gid), Some(client_id)) = (claims.get("gid").and_then(Value::as_str), claims.get("client_id").and_then(Value::as_str)) else {
        return bearer_error("not an MSync access token");
    };
    let grant: Option<(String, String, Option<i64>)> =
        sqlx::query_as("SELECT account_id, scopes, revoked_at FROM oauth_grants WHERE id = ? AND client_id = ?")
            .bind(gid)
            .bind(client_id)
            .fetch_optional(&ctx.pool)
            .await
            .unwrap_or(None);
    let Some((account, scopes, revoked)) = grant else { return bearer_error("unknown grant") };
    if revoked.is_some() && !mutant::on("userinfo_ignores_revocation") {
        return bearer_error("the grant was revoked");
    }
    let Ok(Some(client)) = clients::get(&ctx.pool, client_id).await else { return bearer_error("unknown client") };
    let Ok(Some(member)) = active_member(&ctx.pool, &ctx.cra, &account).await else {
        return bearer_error("not an active member");
    };
    let scopes: Vec<String> = serde_json::from_str(&scopes).unwrap_or_default();
    let mut out = member_claims(&ctx, &client, &member, &scopes);
    out.insert("sub".into(), json!(client.pairwise_sub(&account)));
    HttpResponse::Ok().insert_header(("Cache-Control", "no-store")).json(Value::Object(out))
}

#[get("/oauth/userinfo")]
pub async fn userinfo_get(req: HttpRequest, ctx: web::Data<OAuthContext>) -> HttpResponse {
    userinfo_impl(req, ctx).await
}

#[post("/oauth/userinfo")]
pub async fn userinfo_post(req: HttpRequest, ctx: web::Data<OAuthContext>) -> HttpResponse {
    userinfo_impl(req, ctx).await
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(discovery)
        .service(jwks)
        .service(authorize)
        .service(authorize_consent)
        .service(token)
        .service(revoke)
        .service(userinfo_get)
        .service(userinfo_post);
}
