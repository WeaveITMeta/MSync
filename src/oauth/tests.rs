//! MSync as an authorization server, end to end through the real routes: redirect matching,
//! PKCE, single-use codes, refresh rotation and reuse detection, ID tokens against the JWKS,
//! the claim allowlist, pairwise subjects, suspension, the session and bearer separation,
//! `Origin`, key sealing and rotation, and the full story with the Eustress example CRA.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use actix_web::cookie::Cookie;
use actix_web::http::StatusCode;
use actix_web::test as atest;
use base64::Engine;
use chrono::NaiveDate;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use super::keys::{self, Kek, PublicKey};
use super::*;
use crate::accounts::providers::encode_query;
use crate::accounts::rules::random_token;
use crate::accounts::tests::{
    accounts_ctx, location, pool, query_param, Mocks, RecordingMailer, PUBLIC,
};
use crate::accounts::tests::{callback, sign_in, start};
use crate::auth::{CraDirectory, Directory, EnvDirectory};
use crate::chat::tests::{activate, eustress_files, today, MockStripe};
use crate::cra::loader::{load_commit, CraHandle, ORG_ID};
use crate::cra::{
    ChangeContext, CraFiles, CraRepo, CraSource, FoundryIdentity, LoadOutcome, SourceError,
};
use crate::payments::api::ApiContext;

const B64URL: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;
const ADMIN: &str = "admin-secret";
const REDIRECT: &str = "https://eustress.example/auth/msync/callback";

fn oauth_ctx(pool: SqlitePool, cra: CraHandle) -> OAuthContext {
    let mut k = [0u8; 32];
    getrandom::fill(&mut k).unwrap();
    OAuthContext {
        pool,
        issuer: PUBLIC.into(),
        kek: Some(Kek::from_bytes(k)),
        cra,
        sign_in_providers: vec![crate::accounts::Provider::Github],
        today: Some(today()),
    }
}

fn api_ctx(pool: SqlitePool, cra: CraHandle) -> ApiContext {
    let mut payments = crate::payments::PaymentConfig::from_lookup(|_| None);
    payments.public_url = PUBLIC.into();
    payments.admin_token = Some(ADMIN.into());
    let directory: Arc<dyn Directory> = Arc::new(
        CraDirectory::new(
            EnvDirectory::new(vec!["github:example-founder".to_string()]),
            cra,
        )
        .with_today(today()),
    );
    ApiContext {
        pool,
        payments: Arc::new(payments),
        stripe: Arc::new(MockStripe::default()),
        directory,
    }
}

macro_rules! oapp {
    ($pool:expr, $cra:expr, $mocks:expr) => {
        atest::init_service(
            actix_web::App::new()
                .app_data(actix_web::web::Data::new(accounts_ctx(
                    $pool.clone(),
                    $cra.clone(),
                    &$mocks.url(),
                    Arc::new(RecordingMailer::default()),
                )))
                .app_data(actix_web::web::Data::new(oauth_ctx(
                    $pool.clone(),
                    $cra.clone(),
                )))
                .app_data(actix_web::web::Data::new(api_ctx(
                    $pool.clone(),
                    $cra.clone(),
                )))
                .configure(crate::accounts::routes::configure)
                .configure(crate::oauth::configure)
                .configure(crate::payments::api::configure),
        )
        .await
    };
}

/// Register a client with the bootstrap admin token. Returns (client_id, secret).
macro_rules! register {
    ($app:expr, $body:expr) => {{
        let r = atest::call_service(
            &$app,
            atest::TestRequest::post()
                .uri("/api/v1/oauth/clients")
                .insert_header(("Authorization", format!("Bearer {ADMIN}")))
                .set_json($body)
                .to_request(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::CREATED);
        let v: Value = atest::read_body_json(r).await;
        (
            v["client_id"].as_str().unwrap().to_string(),
            v["client_secret"].as_str().map(str::to_string),
        )
    }};
}

fn client_body(claims: &[&str]) -> Value {
    json!({
        "name": "Eustress Engine",
        "redirect_uris": [REDIRECT],
        "allowed_scopes": ["openid", "profile", "groups", "rank", "merit"],
        "allowed_claims": claims,
        "consumer_key": "eustress-site",
    })
}

const ALL_CLAIMS: &[&str] = &["name", "handle", "groups", "titles", "rank", "merit"];

fn pkce() -> (String, String) {
    let v = random_token().unwrap();
    let c = B64URL.encode(Sha256::digest(v.as_bytes()));
    (v, c)
}

fn authorize_uri(
    client: &str,
    redirect: &str,
    scope: &str,
    state: &str,
    nonce: &str,
    challenge: Option<&str>,
) -> String {
    let mut q = vec![
        ("response_type", "code"),
        ("client_id", client),
        ("redirect_uri", redirect),
        ("scope", scope),
        ("state", state),
        ("nonce", nonce),
    ];
    if let Some(c) = challenge {
        q.push(("code_challenge", c));
        q.push(("code_challenge_method", "S256"));
    }
    format!("/oauth/authorize?{}", encode_query(&q))
}

/// Run authorize (with consent when asked) and return the redirect Location.
macro_rules! authorize {
    ($app:expr, $session:expr, $client:expr, $scope:expr, $nonce:expr, $challenge:expr) => {{
        let uri = authorize_uri($client, REDIRECT, $scope, "st-1", $nonce, Some($challenge));
        let r = atest::call_service(
            &$app,
            atest::TestRequest::get()
                .uri(&uri)
                .cookie($session.clone())
                .to_request(),
        )
        .await;
        if r.status() == StatusCode::OK {
            let html = String::from_utf8(atest::read_body(r).await.to_vec()).unwrap();
            let csrf = html
                .split("name=\"csrf\" value=\"")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap()
                .to_string();
            let form = [
                ("response_type", "code".to_string()),
                ("client_id", $client.to_string()),
                ("redirect_uri", REDIRECT.to_string()),
                ("scope", $scope.to_string()),
                ("state", "st-1".to_string()),
                ("nonce", $nonce.to_string()),
                ("code_challenge", $challenge.to_string()),
                ("code_challenge_method", "S256".to_string()),
                ("csrf", csrf),
                ("decision", "allow".to_string()),
            ];
            let r = atest::call_service(
                &$app,
                atest::TestRequest::post()
                    .uri("/oauth/authorize")
                    .insert_header(("Origin", PUBLIC))
                    .cookie($session.clone())
                    .set_form(form)
                    .to_request(),
            )
            .await;
            assert_eq!(r.status(), StatusCode::FOUND);
            location(&r).unwrap()
        } else {
            assert_eq!(r.status(), StatusCode::FOUND);
            location(&r).unwrap()
        }
    }};
}

/// POST /oauth/token with client_secret_basic (or client_id only for a public client).
macro_rules! token {
    ($app:expr, $client:expr, $secret:expr, $form:expr) => {{
        let mut form: Vec<(String, String)> = $form
            .iter()
            .map(|(k, v): &(&str, String)| (k.to_string(), v.clone()))
            .collect();
        let mut req = atest::TestRequest::post().uri("/oauth/token");
        match $secret {
            Some(s) => {
                req = req.insert_header((
                    "Authorization",
                    format!(
                        "Basic {}",
                        base64::engine::general_purpose::STANDARD
                            .encode(format!("{}:{}", $client, s))
                    ),
                ));
            }
            None => form.push(("client_id".into(), $client.to_string())),
        }
        let r = atest::call_service(&$app, req.set_form(form).to_request()).await;
        let status = r.status();
        let v: Value = atest::read_body_json(r).await;
        (status, v)
    }};
}

macro_rules! exchange {
    ($app:expr, $client:expr, $secret:expr, $code:expr, $verifier:expr) => {
        token!(
            $app,
            $client,
            $secret,
            [
                ("grant_type", "authorization_code".to_string()),
                ("code", $code.to_string()),
                ("redirect_uri", REDIRECT.to_string()),
                ("code_verifier", $verifier.to_string()),
            ]
        )
    };
}

macro_rules! refresh {
    ($app:expr, $client:expr, $secret:expr, $rt:expr) => {
        token!(
            $app,
            $client,
            $secret,
            [
                ("grant_type", "refresh_token".to_string()),
                ("refresh_token", $rt.to_string())
            ]
        )
    };
}

macro_rules! userinfo {
    ($app:expr, $at:expr) => {{
        let r = atest::call_service(&$app, atest::TestRequest::get().uri("/oauth/userinfo").insert_header(("Authorization", format!("Bearer {}", $at))).to_request()).await;
        let s = r.status();
        let v: Value = atest::read_body_json(r).await;
        (s, v)
    }};
}

macro_rules! jwks {
    ($app:expr) => {{
        let r = atest::call_service(
            &$app,
            atest::TestRequest::get().uri("/oauth/jwks").to_request(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let v: Value = atest::read_body_json(r).await;
        v["keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| PublicKey::from_jwk(k).unwrap())
            .collect::<Vec<_>>()
    }};
}

/// A member signed in with GitHub, a registered client, and a code ready to exchange.
struct Flow {
    session: Cookie<'static>,
    client: String,
    secret: Option<String>,
    code: String,
    verifier: String,
}

macro_rules! flow {
    ($app:expr, $mocks:expr, $code:expr, $gh_id:expr, $login:expr, $claims:expr) => {{
        $mocks.github($code, $gh_id, $login, None).await;
        let session = sign_in!($app, "github", $code);
        let (client, secret) = register!($app, client_body($claims));
        let (verifier, challenge) = pkce();
        let loc = authorize!(
            $app,
            session,
            &client,
            "openid profile groups rank merit",
            "n-123",
            &challenge
        );
        assert!(loc.starts_with(REDIRECT), "{loc}");
        assert_eq!(query_param(&loc, "state").as_deref(), Some("st-1"));
        let code = query_param(&loc, "code").unwrap_or_else(|| panic!("no code in {loc}"));
        Flow {
            session,
            client,
            secret,
            code,
            verifier,
        }
    }};
}

async fn setup() -> (SqlitePool, CraHandle, Mocks) {
    let pool = pool().await;
    let cra = CraHandle::new();
    activate(&cra, &eustress_files());
    (pool, cra, Mocks::new().await)
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

// ----------------------------------------------------------- redirect URIs

#[tokio::test]
async fn an_unregistered_or_inexact_redirect_uri_is_refused_without_redirecting() {
    let (pool, cra, mut m) = setup().await;
    let app = oapp!(pool, cra, m);
    m.github("gh1", 1004, "bo-example", None).await;
    let s = sign_in!(app, "github", "gh1");
    let (client, _) = register!(app, client_body(ALL_CLAIMS));
    let (_, challenge) = pkce();
    for bad in [
        "https://evil.example/cb",
        "https://eustress.example/auth/msync/callback/extra",
        "https://eustress.example/auth/msync/callback?next=https://evil.example",
        "https://EUSTRESS.example/auth/msync/callback",
        "https://eustress.example/auth/msync/callbac",
        "",
    ] {
        let uri = authorize_uri(&client, bad, "openid", "st", "n", Some(&challenge));
        let r = atest::call_service(
            &app,
            atest::TestRequest::get()
                .uri(&uri)
                .cookie(s.clone())
                .to_request(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{bad}");
        assert!(location(&r).is_none(), "never redirect to {bad}");
    }
    // Missing redirect_uri, and an unknown client: no redirect either.
    let r = atest::call_service(
        &app,
        atest::TestRequest::get()
            .uri(&format!(
                "/oauth/authorize?response_type=code&client_id={client}&scope=openid"
            ))
            .cookie(s.clone())
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert!(location(&r).is_none());
    let uri = authorize_uri(
        "msc_unknown",
        REDIRECT,
        "openid",
        "st",
        "n",
        Some(&challenge),
    );
    let r = atest::call_service(
        &app,
        atest::TestRequest::get()
            .uri(&uri)
            .cookie(s.clone())
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert!(location(&r).is_none());

    // With the exact registered URI, protocol errors DO go back to it, with the state.
    let uri = authorize_uri(&client, REDIRECT, "openid", "st", "n", None);
    let r = atest::call_service(
        &app,
        atest::TestRequest::get()
            .uri(&uri)
            .cookie(s.clone())
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::FOUND);
    let loc = location(&r).unwrap();
    assert!(loc.starts_with(REDIRECT));
    assert_eq!(
        query_param(&loc, "error").as_deref(),
        Some("invalid_request"),
        "PKCE is required"
    );
    assert_eq!(query_param(&loc, "state").as_deref(), Some("st"));
    let uri = authorize_uri(
        &client,
        REDIRECT,
        "openid admin",
        "st",
        "n",
        Some(&challenge),
    );
    let r = atest::call_service(
        &app,
        atest::TestRequest::get()
            .uri(&uri)
            .cookie(s.clone())
            .to_request(),
    )
    .await;
    assert_eq!(
        query_param(&location(&r).unwrap(), "error").as_deref(),
        Some("invalid_scope")
    );

    // Registration refuses anything but exact https (or loopback http) URIs.
    for bad in [
        "http://eustress.example/cb",
        "https://*.eustress.example/cb",
        "https://eustress.example/cb#x",
    ] {
        let r = atest::call_service(
            &app,
            atest::TestRequest::post()
                .uri("/api/v1/oauth/clients")
                .insert_header(("Authorization", format!("Bearer {ADMIN}")))
                .set_json(
                    json!({"name": "x", "redirect_uris": [bad], "allowed_scopes": ["openid"]}),
                )
                .to_request(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
}

// ----------------------------------------------------------------- PKCE

#[tokio::test]
async fn a_missing_or_wrong_pkce_verifier_is_refused() {
    let (pool, cra, mut m) = setup().await;
    let app = oapp!(pool, cra, m);
    let f = flow!(app, m, "gh1", 1004, "bo-example", ALL_CLAIMS);
    let (missing_status, v) = token!(
        app,
        f.client,
        f.secret.as_deref(),
        [
            ("grant_type", "authorization_code".to_string()),
            ("code", f.code.clone()),
            ("redirect_uri", REDIRECT.to_string())
        ]
    );
    assert_eq!(missing_status, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"], "invalid_grant");

    // A failed attempt used the code up; get another and present the wrong verifier.
    let (_, challenge) = pkce();
    let loc = authorize!(app, f.session, &f.client, "openid", "n", &challenge);
    let code = query_param(&loc, "code").unwrap();
    let (wrong, _) = pkce();
    let (s, v) = exchange!(app, f.client, f.secret.as_deref(), code, wrong);
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
    assert!(v.get("access_token").is_none());

    // A public client gets no secret and must use PKCE too.
    let r = atest::call_service(
        &app,
        atest::TestRequest::post()
            .uri("/api/v1/oauth/clients")
            .insert_header(("Authorization", format!("Bearer {ADMIN}")))
            .set_json(json!({"name": "Desktop", "public": true, "redirect_uris": [REDIRECT], "allowed_scopes": ["openid"]}))
            .to_request(),
    )
    .await;
    let v: Value = atest::read_body_json(r).await;
    assert!(v["client_secret"].is_null());
    let public = v["client_id"].as_str().unwrap().to_string();
    let (verifier, challenge) = pkce();
    let code = query_param(
        &authorize!(app, f.session, &public, "openid", "n", &challenge),
        "code",
    )
    .unwrap();
    let (s, _) = exchange!(app, public, None::<&str>, code, "x".repeat(43));
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let code = query_param(
        &authorize!(app, f.session, &public, "openid", "n", &challenge),
        "code",
    )
    .unwrap();
    let (s, v) = exchange!(app, public, None::<&str>, code, verifier);
    assert_eq!(s, StatusCode::OK, "{v}");
}

// ------------------------------------------------------------------ codes

#[tokio::test]
async fn a_code_used_twice_is_refused_and_its_tokens_are_revoked() {
    let (pool, cra, mut m) = setup().await;
    let app = oapp!(pool, cra, m);
    let f = flow!(app, m, "gh1", 1004, "bo-example", ALL_CLAIMS);
    let (s, first) = exchange!(app, f.client, f.secret.as_deref(), f.code, f.verifier);
    assert_eq!(s, StatusCode::OK, "{first}");
    let at = first["access_token"].as_str().unwrap();
    assert_eq!(userinfo!(app, at).0, StatusCode::OK);

    let (s, again) = exchange!(app, f.client, f.secret.as_deref(), f.code, f.verifier);
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(again["error"], "invalid_grant");
    assert!(again.get("access_token").is_none());
    // Everything the first exchange issued is now dead.
    assert_eq!(userinfo!(app, at).0, StatusCode::UNAUTHORIZED);
    let (s, v) = refresh!(
        app,
        f.client,
        f.secret.as_deref(),
        first["refresh_token"].as_str().unwrap()
    );
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
}

#[tokio::test]
async fn a_code_for_another_client_is_refused() {
    let (pool, cra, mut m) = setup().await;
    let app = oapp!(pool, cra, m);
    let f = flow!(app, m, "gh1", 1004, "bo-example", ALL_CLAIMS);
    let (other, other_secret) = register!(app, client_body(ALL_CLAIMS));
    let (s, v) = exchange!(app, other, other_secret.as_deref(), f.code, f.verifier);
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
    // A wrong secret for the right client is invalid_client.
    let (s, v) = exchange!(app, f.client, Some("mss_wrong"), f.code, f.verifier);
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("invalid_client"))
    );
    // The code still works for its own client.
    let (s, _) = exchange!(app, f.client, f.secret.as_deref(), f.code, f.verifier);
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn a_code_expires_after_sixty_seconds() {
    let (pool, cra, mut m) = setup().await;
    let app = oapp!(pool, cra, m);
    let f = flow!(app, m, "gh1", 1004, "bo-example", ALL_CLAIMS);
    let (exp, created): (i64, i64) =
        sqlx::query_as("SELECT expires_at, created_at FROM oauth_codes")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(exp - created, 60);
    sqlx::query("UPDATE oauth_codes SET expires_at = ?")
        .bind(now() - 1)
        .execute(&pool)
        .await
        .unwrap();
    let (s, v) = exchange!(app, f.client, f.secret.as_deref(), f.code, f.verifier);
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
}

// ---------------------------------------------------------------- refresh

#[tokio::test]
async fn refresh_tokens_rotate_and_reuse_revokes_the_family() {
    let (pool, cra, mut m) = setup().await;
    let app = oapp!(pool, cra, m);
    let f = flow!(app, m, "gh1", 1004, "bo-example", ALL_CLAIMS);
    let (_, t0) = exchange!(app, f.client, f.secret.as_deref(), f.code, f.verifier);
    let r1 = t0["refresh_token"].as_str().unwrap().to_string();
    let (s, t1) = refresh!(app, f.client, f.secret.as_deref(), r1);
    assert_eq!(s, StatusCode::OK, "{t1}");
    let r2 = t1["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(r1, r2, "rotated");
    let (s, t2) = refresh!(app, f.client, f.secret.as_deref(), r2);
    assert_eq!(s, StatusCode::OK);
    let r3 = t2["refresh_token"].as_str().unwrap().to_string();
    let at3 = t2["access_token"].as_str().unwrap().to_string();
    assert_eq!(userinfo!(app, at3).0, StatusCode::OK);

    // Someone replays r1 (stolen earlier): refused, and the whole family dies.
    let (s, v) = refresh!(app, f.client, f.secret.as_deref(), r1);
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
    let (s, _) = refresh!(app, f.client, f.secret.as_deref(), r3);
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "the newest refresh token is revoked too"
    );
    assert_eq!(userinfo!(app, at3).0, StatusCode::UNAUTHORIZED);
    // Refresh tokens are stored hashed.
    let raw: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM oauth_refresh_tokens WHERE token_hash = ?")
            .bind(&r3)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(raw, 0);
}

#[tokio::test]
async fn revocation_endpoint_kills_the_family() {
    let (pool, cra, mut m) = setup().await;
    let app = oapp!(pool, cra, m);
    let f = flow!(app, m, "gh1", 1004, "bo-example", ALL_CLAIMS);
    let (_, t0) = exchange!(app, f.client, f.secret.as_deref(), f.code, f.verifier);
    let basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!(
            "{}:{}",
            f.client,
            f.secret.clone().unwrap()
        ))
    );
    let r = atest::call_service(
        &app,
        atest::TestRequest::post()
            .uri("/oauth/revoke")
            .insert_header(("Authorization", basic))
            .set_form([("token", t0["refresh_token"].as_str().unwrap())])
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        userinfo!(app, t0["access_token"].as_str().unwrap()).0,
        StatusCode::UNAUTHORIZED
    );
}

// ------------------------------------------------------------- ID tokens

fn tamper(token: &str) -> String {
    let parts: Vec<&str> = token.split('.').collect();
    let mut claims: Value = serde_json::from_slice(&B64URL.decode(parts[1]).unwrap()).unwrap();
    claims["groups"] = json!(["lead-maintainer"]);
    format!(
        "{}.{}.{}",
        parts[0],
        B64URL.encode(claims.to_string()),
        parts[2]
    )
}

#[tokio::test]
async fn the_id_token_verifies_against_the_jwks_and_fails_after_tampering() {
    let (pool, cra, mut m) = setup().await;
    let app = oapp!(pool, cra, m);
    let f = flow!(app, m, "gh1", 1004, "bo-example", ALL_CLAIMS);
    let (_, t) = exchange!(app, f.client, f.secret.as_deref(), f.code, f.verifier);
    let id = t["id_token"].as_str().unwrap();
    let keys = jwks!(app);
    let claims =
        jwt::verify_claims(id, &keys, "JWT", PUBLIC, Some(&f.client), now()).expect("verifies");
    assert_eq!(claims["iss"], PUBLIC);
    assert_eq!(claims["aud"], f.client.as_str());
    assert_eq!(claims["nonce"], "n-123", "nonce echoed");
    let exp = claims["exp"].as_i64().unwrap();
    let iat = claims["iat"].as_i64().unwrap();
    assert!(
        exp > now() && exp - iat == ID_TTL_SECS && ID_TTL_SECS <= 600,
        "short lifetime"
    );
    assert_eq!(
        jwt::verify(&tamper(id), &keys).unwrap_err(),
        jwt::JwtError::Signature
    );
    assert_eq!(
        jwt::verify_claims(id, &keys, "JWT", PUBLIC, Some("msc_other"), now()).unwrap_err(),
        jwt::JwtError::Audience
    );
    assert_eq!(
        jwt::verify_claims(id, &keys, "JWT", "https://evil.example", None, now()).unwrap_err(),
        jwt::JwtError::Issuer
    );
    assert_eq!(
        jwt::verify_claims(id, &keys, "JWT", PUBLIC, None, exp).unwrap_err(),
        jwt::JwtError::Expired
    );
    // An access token is not an ID token, and the userinfo endpoint refuses a tampered one.
    let at = t["access_token"].as_str().unwrap();
    assert_eq!(
        jwt::verify_claims(at, &keys, "JWT", PUBLIC, None, now()).unwrap_err(),
        jwt::JwtError::Type
    );
    assert_eq!(userinfo!(app, tamper(at)).0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        userinfo!(app, id).0,
        StatusCode::UNAUTHORIZED,
        "an ID token is not an access token"
    );
}

#[tokio::test]
async fn a_claim_outside_the_clients_allowlist_is_absent() {
    let (pool, cra, mut m) = setup().await;
    let app = oapp!(pool, cra, m);
    let f = flow!(app, m, "gh1", 1004, "bo-example", &["groups"]);
    let (_, t) = exchange!(app, f.client, f.secret.as_deref(), f.code, f.verifier);
    let keys = jwks!(app);
    let claims = jwt::verify_claims(
        t["id_token"].as_str().unwrap(),
        &keys,
        "JWT",
        PUBLIC,
        Some(&f.client),
        now(),
    )
    .unwrap();
    assert_eq!(claims["groups"], json!(["reviewer", "newcomer"]));
    for absent in ["name", "handle", "titles", "rank", "merit"] {
        assert!(
            claims.get(absent).is_none(),
            "{absent} must be absent: {claims}"
        );
    }
    let (_, info) = userinfo!(app, t["access_token"].as_str().unwrap());
    assert_eq!(info["groups"], json!(["reviewer", "newcomer"]));
    assert!(info.get("name").is_none() && info.get("rank").is_none());
}

#[tokio::test]
async fn two_clients_get_different_subjects_for_one_member() {
    let (pool, cra, mut m) = setup().await;
    let app = oapp!(pool, cra, m);
    let f = flow!(app, m, "gh1", 1004, "bo-example", ALL_CLAIMS);
    let (_, ta) = exchange!(app, f.client, f.secret.as_deref(), f.code, f.verifier);
    let (b, b_secret) = register!(app, client_body(ALL_CLAIMS));
    let (verifier, challenge) = pkce();
    let code = query_param(
        &authorize!(app, f.session, &b, "openid", "n", &challenge),
        "code",
    )
    .unwrap();
    let (_, tb) = exchange!(app, b, b_secret.as_deref(), code, verifier);
    let keys = jwks!(app);
    let sa = jwt::verify(ta["id_token"].as_str().unwrap(), &keys)
        .unwrap()
        .1["sub"]
        .clone();
    let sb = jwt::verify(tb["id_token"].as_str().unwrap(), &keys)
        .unwrap()
        .1["sub"]
        .clone();
    assert_ne!(sa, sb, "pairwise");
    // Stable per client, and the same in userinfo.
    assert_eq!(
        userinfo!(app, ta["access_token"].as_str().unwrap()).1["sub"],
        sa
    );
    assert_eq!(
        userinfo!(app, tb["access_token"].as_str().unwrap()).1["sub"],
        sb
    );
    let account: String = sqlx::query_scalar("SELECT id FROM accounts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_ne!(sa, json!(account), "never the account id");
}

// ------------------------------------------------------- separation of channels

#[tokio::test]
async fn a_session_cookie_does_not_open_api_v1_and_a_bearer_token_does_not_open_authorize() {
    let (pool, cra, mut m) = setup().await;
    let app = oapp!(pool, cra, m);
    m.github("gh1", 1001, "example-founder", None).await;
    let s = sign_in!(app, "github", "gh1");
    // The owner's own browser session opens nothing under /api/v1.
    let r = atest::call_service(
        &app,
        atest::TestRequest::get()
            .uri("/api/v1/payouts/policy")
            .cookie(s.clone())
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = atest::call_service(
        &app,
        atest::TestRequest::post()
            .uri("/api/v1/oauth/clients")
            .cookie(s.clone())
            .set_json(client_body(ALL_CLAIMS))
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

    // An API token (even the bootstrap admin token, even the session's own token as a bearer)
    // does not open /oauth/authorize.
    let (client, _) = register!(app, client_body(ALL_CLAIMS));
    let (_, challenge) = pkce();
    let uri = authorize_uri(&client, REDIRECT, "openid", "st", "n", Some(&challenge));
    for bearer in [ADMIN.to_string(), s.value().to_string()] {
        let r = atest::call_service(
            &app,
            atest::TestRequest::get()
                .uri(&uri)
                .insert_header(("Authorization", format!("Bearer {bearer}")))
                .to_request(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        assert!(location(&r).is_none());
    }
    // The cookie does.
    let r = atest::call_service(
        &app,
        atest::TestRequest::get()
            .uri(&uri)
            .cookie(s.clone())
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK, "the consent page");
}

#[tokio::test]
async fn a_cross_origin_consent_post_is_refused() {
    let (pool, cra, mut m) = setup().await;
    let app = oapp!(pool, cra, m);
    m.github("gh1", 1004, "bo-example", None).await;
    let s = sign_in!(app, "github", "gh1");
    let (client, _) = register!(app, client_body(ALL_CLAIMS));
    let (_, challenge) = pkce();
    let form = [
        ("response_type", "code".to_string()),
        ("client_id", client.clone()),
        ("redirect_uri", REDIRECT.to_string()),
        ("scope", "openid".to_string()),
        ("code_challenge", challenge.clone()),
        ("code_challenge_method", "S256".to_string()),
        ("csrf", crate::accounts::rules::consent_nonce(s.value())),
        ("decision", "allow".to_string()),
    ];
    let r = atest::call_service(
        &app,
        atest::TestRequest::post()
            .uri("/oauth/authorize")
            .insert_header(("Origin", "https://evil.example"))
            .cookie(s.clone())
            .set_form(form.clone())
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert!(location(&r).is_none());
    let codes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oauth_codes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(codes, 0);
    // Same origin but a forged csrf value: refused too.
    let mut forged = form.clone();
    forged[6].1 = "0".repeat(32);
    let r = atest::call_service(
        &app,
        atest::TestRequest::post()
            .uri("/oauth/authorize")
            .insert_header(("Origin", PUBLIC))
            .cookie(s.clone())
            .set_form(forged)
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn consent_is_remembered_per_account_and_client() {
    let (pool, cra, mut m) = setup().await;
    let app = oapp!(pool, cra, m);
    let f = flow!(app, m, "gh1", 1004, "bo-example", ALL_CLAIMS);
    // Second time: straight to a code, no consent page.
    let (_, challenge) = pkce();
    let uri = authorize_uri(
        &f.client,
        REDIRECT,
        "openid groups",
        "st",
        "n",
        Some(&challenge),
    );
    let r = atest::call_service(
        &app,
        atest::TestRequest::get()
            .uri(&uri)
            .cookie(f.session.clone())
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::FOUND);
    assert!(query_param(&location(&r).unwrap(), "code").is_some());
}

#[tokio::test]
async fn a_signed_in_non_member_gets_no_code() {
    let (pool, cra, mut m) = setup().await;
    let app = oapp!(pool, cra, m);
    m.github("gh1", 999, "stranger", None).await;
    let s = sign_in!(app, "github", "gh1");
    let (client, _) = register!(app, client_body(ALL_CLAIMS));
    let (_, challenge) = pkce();
    let r = atest::call_service(
        &app,
        atest::TestRequest::get()
            .uri(&authorize_uri(
                &client,
                REDIRECT,
                "openid",
                "st",
                "n",
                Some(&challenge),
            ))
            .cookie(s)
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::FOUND);
    assert_eq!(
        query_param(&location(&r).unwrap(), "error").as_deref(),
        Some("access_denied")
    );
}

// ------------------------------------------------------------------- keys

#[tokio::test]
async fn the_signing_key_is_sealed_and_rotates_without_breaking_live_tokens() {
    let (pool, cra, mut m) = setup().await;
    let app = oapp!(pool, cra, m);
    let f = flow!(app, m, "gh1", 1004, "bo-example", ALL_CLAIMS);
    let (_, t) = exchange!(app, f.client, f.secret.as_deref(), f.code, f.verifier);
    let old_id = t["id_token"].as_str().unwrap().to_string();
    let (kid, sealed): (String, String) =
        sqlx::query_as("SELECT kid, private_key_sealed FROM oauth_signing_keys")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!sealed.contains("PRIVATE"));

    let r = atest::call_service(
        &app,
        atest::TestRequest::post()
            .uri("/api/v1/oauth/keys/rotate")
            .insert_header(("Authorization", format!("Bearer {ADMIN}")))
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let v: Value = atest::read_body_json(r).await;
    let new_kid = v["kid"].as_str().unwrap().to_string();
    assert_ne!(new_kid, kid);
    let keys = jwks!(app);
    assert_eq!(keys.len(), 2, "the retired key stays published");
    assert!(
        jwt::verify(&old_id, &keys).is_ok(),
        "a token signed before rotation still verifies"
    );
    let (_, t2) = refresh!(
        app,
        f.client,
        f.secret.as_deref(),
        t["refresh_token"].as_str().unwrap()
    );
    let (h, _) = jwt::verify(t2["id_token"].as_str().unwrap(), &keys).unwrap();
    assert_eq!(h["kid"], new_kid.as_str(), "the new key signs");
    // After the publication window the retired key is gone.
    assert_eq!(
        keys::published(&pool, now() + RETIRED_KEY_PUBLISH_SECS + 1)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn a_sealed_key_opens_only_under_its_kek_and_key_id() {
    let a = Kek::from_bytes([7u8; 32]);
    let b = Kek::from_bytes([8u8; 32]);
    let sealed = a.seal("msk_1", b"pkcs8 bytes").unwrap();
    assert_eq!(a.open("msk_1", &sealed).unwrap(), b"pkcs8 bytes");
    assert!(b.open("msk_1", &sealed).is_err(), "another KEK");
    assert!(a.open("msk_2", &sealed).is_err(), "another key id");
    assert!(Kek::parse(&"ab".repeat(32)).is_some());
    assert!(Kek::parse("too short").is_none());
}

#[tokio::test]
async fn without_a_key_encryption_key_the_provider_is_off() {
    let pool = pool().await;
    let mut ctx = oauth_ctx(pool, CraHandle::new());
    ctx.kek = None;
    let app = atest::init_service(
        actix_web::App::new()
            .app_data(actix_web::web::Data::new(ctx))
            .configure(server::configure),
    )
    .await;
    for uri in [
        "/.well-known/openid-configuration",
        "/oauth/jwks",
        "/oauth/authorize?client_id=x",
    ] {
        let r = atest::call_service(&app, atest::TestRequest::get().uri(uri).to_request()).await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE, "{uri}");
    }
}

#[tokio::test]
async fn discovery_names_the_endpoints() {
    let (pool, cra, m) = setup().await;
    let app = oapp!(pool, cra, m);
    let r = atest::call_service(
        &app,
        atest::TestRequest::get()
            .uri("/.well-known/openid-configuration")
            .to_request(),
    )
    .await;
    let v: Value = atest::read_body_json(r).await;
    assert_eq!(v["issuer"], PUBLIC);
    assert_eq!(v["token_endpoint"], format!("{PUBLIC}/oauth/token"));
    assert_eq!(v["jwks_uri"], format!("{PUBLIC}/oauth/jwks"));
    assert_eq!(v["subject_types_supported"], json!(["pairwise"]));
    assert_eq!(v["code_challenge_methods_supported"], json!(["S256"]));
    assert_eq!(v["id_token_signing_alg_values_supported"], json!(["ES256"]));
}

// ------------------------------------------------ suspension and the full story

const CRA_REPO: &str = "example-eustress/cra";

struct FakeSource {
    repo: CraRepo,
    commits: Mutex<HashMap<String, CraFiles>>,
}

#[async_trait::async_trait]
impl CraSource for FakeSource {
    fn repo(&self) -> &CraRepo {
        &self.repo
    }
    async fn fetch_files(&self, sha: &str) -> Result<CraFiles, SourceError> {
        self.commits
            .lock()
            .unwrap()
            .get(sha)
            .cloned()
            .ok_or_else(|| SourceError::Api(format!("no commit {sha}")))
    }
    async fn change_context(
        &self,
        _sha: &str,
        _p: Option<&FoundryIdentity>,
        today: NaiveDate,
    ) -> Result<ChangeContext, SourceError> {
        Ok(ChangeContext {
            actor: FoundryIdentity::github("example-founder"),
            via_pull_request: false,
            author: None,
            approvers: vec![],
            today,
        })
    }
    async fn branch_head(&self, _b: &str) -> Result<String, SourceError> {
        Err(SourceError::Api("no head".into()))
    }
}

fn with_status(files: &CraFiles, member: &str, status: &str) -> CraFiles {
    let at = files.members.find(&format!("id = \"{member}\"")).unwrap();
    let rel = files.members[at..].find("status = \"active\"").unwrap();
    let mut f = files.clone();
    f.members.replace_range(
        at + rel..at + rel + "status = \"active\"".len(),
        &format!("status = \"{status}\""),
    );
    f
}

async fn load(pool: &SqlitePool, src: &FakeSource, cra: &CraHandle, sha: &str) {
    let owners = vec!["github:example-founder".to_string()];
    let out = load_commit(pool, src, &src.repo, sha, None, &owners, today())
        .await
        .unwrap();
    assert!(matches!(out, LoadOutcome::Activated { .. }), "{out:?}");
    cra.reload_from_store(pool, ORG_ID).await.unwrap();
}

/// The story: a member signs in with GitHub, a platform runs the authorization code flow with
/// PKCE, the ID token's `groups` are that member's roles in the Eustress example CRA, and a
/// new snapshot that suspends the member kills their refresh token and every new attempt.
#[tokio::test]
async fn full_story_sign_in_platform_groups_then_suspension() {
    let pool = pool().await;
    let cra = CraHandle::new();
    let src = FakeSource {
        repo: CraRepo::new(
            crate::foundry::FoundryProvider::GitHub,
            "github.com",
            CRA_REPO,
        ),
        commits: Mutex::new(HashMap::new()),
    };
    let v1 = eustress_files();
    let v2 = with_status(&v1, "bo-example", "suspended");
    src.commits
        .lock()
        .unwrap()
        .insert(format!("{:040x}", 1), v1.clone());
    src.commits
        .lock()
        .unwrap()
        .insert(format!("{:040x}", 2), v2);
    load(&pool, &src, &cra, &format!("{:040x}", 1)).await;

    let mut m = Mocks::new().await;
    let app = oapp!(pool, cra, m);
    // 1. Bo signs in with GitHub (numeric id 1004, login bo-example).
    let f = flow!(app, m, "gh-bo", 1004, "bo-example", ALL_CLAIMS);
    // 2. The platform exchanges the code with its PKCE verifier.
    let (s, t) = exchange!(app, f.client, f.secret.as_deref(), f.code, f.verifier);
    assert_eq!(s, StatusCode::OK, "{t}");
    // 3. The ID token verifies against the JWKS and its groups are Bo's CRA roles.
    let keys = jwks!(app);
    let claims = jwt::verify_claims(
        t["id_token"].as_str().unwrap(),
        &keys,
        "JWT",
        PUBLIC,
        Some(&f.client),
        now(),
    )
    .unwrap();
    let snap = cra.active().unwrap().snapshot.clone();
    let st =
        crate::cra::standing(&snap, "bo-example", cra.merit_of("bo-example"), today()).unwrap();
    let expected: Vec<String> = st
        .positions
        .iter()
        .map(|p| p.key.clone())
        .chain(st.tier.iter().map(|t| t.key.clone()))
        .collect();
    assert_eq!(claims["groups"], json!(expected));
    assert_eq!(claims["groups"], json!(["reviewer", "newcomer"]));
    assert_eq!(claims["titles"], json!(st.titles));
    assert_eq!(claims["rank"], 40);
    assert_eq!(claims["name"], "Bo Example");
    assert_eq!(claims["handle"], "bo-example");
    let (s, info) = userinfo!(app, t["access_token"].as_str().unwrap());
    assert_eq!(s, StatusCode::OK);
    assert_eq!(info["groups"], claims["groups"]);
    assert_eq!(info["sub"], claims["sub"]);

    // 4. A reviewed snapshot suspends Bo. Activation revokes the token family at once.
    load(&pool, &src, &cra, &format!("{:040x}", 2)).await;
    let live: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM oauth_refresh_tokens WHERE revoked_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(live, 0, "activation revoked the refresh tokens");
    let reason: Option<String> = sqlx::query_scalar("SELECT revoked_reason FROM oauth_grants")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(reason.unwrap().contains("not active"));
    // 5. The refresh fails, userinfo fails, and a new sign-in to the platform is denied.
    let (s, v) = refresh!(
        app,
        f.client,
        f.secret.as_deref(),
        t["refresh_token"].as_str().unwrap()
    );
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
    assert_eq!(
        userinfo!(app, t["access_token"].as_str().unwrap()).0,
        StatusCode::UNAUTHORIZED
    );
    let (_, challenge) = pkce();
    let r = atest::call_service(
        &app,
        atest::TestRequest::get()
            .uri(&authorize_uri(
                &f.client,
                REDIRECT,
                "openid",
                "st",
                "n",
                Some(&challenge),
            ))
            .cookie(f.session.clone())
            .to_request(),
    )
    .await;
    assert_eq!(
        query_param(&location(&r).unwrap(), "error").as_deref(),
        Some("access_denied")
    );
}

/// A code issued before suspension cannot be redeemed after it.
#[tokio::test]
async fn a_suspended_member_gets_no_token() {
    let (pool, cra, mut m) = setup().await;
    let app = oapp!(pool, cra, m);
    let f = flow!(app, m, "gh1", 1004, "bo-example", ALL_CLAIMS);
    activate(
        &cra,
        &with_status(&eustress_files(), "bo-example", "suspended"),
    );
    let (s, v) = exchange!(app, f.client, f.secret.as_deref(), f.code, f.verifier);
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
    assert!(v.get("access_token").is_none() && v.get("id_token").is_none());
}
