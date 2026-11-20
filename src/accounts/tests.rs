//! Accounts and sign-in against a real database and the real HTTP provider client pointed at
//! a mock server: linking through the store, state and browser binding, disabled providers,
//! provider errors, sessions, cookie flags, `Origin`, email links, and verification flowing
//! outward to chat bindings and `payout-setup`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use actix_web::cookie::Cookie;
use actix_web::http::StatusCode;
use actix_web::{test as atest, web, App};
use async_trait::async_trait;
use mockito::{Matcher, Mock, ServerGuard};
use serde_json::{json, Value};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};

use super::providers::ProviderConfig;
use super::rules::Provider;
use super::store::{self, member_for_account};
use super::*;
use crate::chat::tests::{activate, eustress_files, today};
use crate::chat::{commands, identity, ChatActor, ChatPlatform};
use crate::cra::loader::CraHandle;

pub(crate) const PUBLIC: &str = "https://msync.example";

pub(crate) async fn pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    pool
}

/// Records every link the mail sender was asked to send.
#[derive(Default)]
pub(crate) struct RecordingMailer {
    pub sent: Mutex<Vec<(String, String)>>,
}

#[async_trait]
impl MailSender for RecordingMailer {
    async fn send_sign_in_link(&self, to: &str, url: &str) -> Result<(), String> {
        self.sent
            .lock()
            .unwrap()
            .push((to.to_string(), url.to_string()));
        Ok(())
    }
}

/// A mock provider server. Each user is keyed by the authorization code that signs them in.
pub(crate) struct Mocks {
    pub server: ServerGuard,
    mocks: Vec<Mock>,
}

impl Mocks {
    pub async fn new() -> Self {
        Mocks {
            server: mockito::Server::new_async().await,
            mocks: Vec::new(),
        }
    }

    pub fn url(&self) -> String {
        self.server.url()
    }

    async fn token(&mut self, path: &str, code: &str, token: &str) {
        let m = self
            .server
            .mock("POST", path)
            .match_body(Matcher::UrlEncoded("code".into(), code.into()))
            .with_header("content-type", "application/json")
            .with_body(json!({"access_token": token, "token_type": "bearer"}).to_string())
            .create_async()
            .await;
        self.mocks.push(m);
    }

    async fn get(&mut self, path: &str, token: &str, body: Value) {
        let m = self
            .server
            .mock("GET", path)
            .match_header("authorization", format!("Bearer {token}").as_str())
            .with_header("content-type", "application/json")
            .with_body(body.to_string())
            .create_async()
            .await;
        self.mocks.push(m);
    }

    /// A GitHub user (numeric id, login, verified emails).
    pub async fn github(&mut self, code: &str, id: u64, login: &str, verified_email: Option<&str>) {
        let tok = format!("gho_{code}");
        self.token("/login/oauth/access_token", code, &tok).await;
        self.get(
            "/user",
            &tok,
            json!({"id": id, "login": login, "name": login}),
        )
        .await;
        let emails = match verified_email {
            Some(e) => {
                json!([{"email": e, "verified": true, "primary": true}, {"email": "unverified@x.io", "verified": false, "primary": false}])
            }
            None => json!([]),
        };
        self.get("/user/emails", &tok, emails).await;
    }

    pub async fn gitlab(&mut self, code: &str, id: u64, login: &str) {
        let tok = format!("glo_{code}");
        self.token("/oauth/token", code, &tok).await;
        self.get("/api/v4/user", &tok, json!({"id": id, "username": login, "name": login, "email": null, "confirmed_at": null})).await;
    }

    pub async fn discord(&mut self, code: &str, id: &str, email: Option<&str>, verified: bool) {
        let tok = format!("dco_{code}");
        self.token("/api/oauth2/token", code, &tok).await;
        self.get("/api/users/@me", &tok, json!({"id": id, "username": format!("user{id}"), "email": email, "verified": verified})).await;
    }

    pub async fn slack(&mut self, code: &str, team: &str, user: &str) {
        let tok = format!("xoxp_{code}");
        self.token("/api/openid.connect.token", code, &tok).await;
        self.get(
            "/api/openid.connect.userInfo",
            &tok,
            json!({"ok": true, "sub": user, "https://slack.com/user_id": user, "https://slack.com/team_id": team, "name": user, "email_verified": false}),
        )
        .await;
    }

    pub async fn google(&mut self, code: &str, sub: &str, email: &str, verified: bool) {
        let tok = format!("ya29_{code}");
        self.token("/token", code, &tok).await;
        self.get(
            "/v1/userinfo",
            &tok,
            json!({"sub": sub, "email": email, "email_verified": verified, "name": "G"}),
        )
        .await;
    }
}

pub(crate) fn accounts_ctx(
    pool: SqlitePool,
    cra: CraHandle,
    base: &str,
    mailer: Arc<RecordingMailer>,
) -> AccountsContext {
    let providers: HashMap<Provider, ProviderConfig> = Provider::OAUTH
        .into_iter()
        .map(|p| (p, ProviderConfig::for_test(p, base)))
        .collect();
    AccountsContext {
        pool,
        public_url: PUBLIC.into(),
        providers,
        api: Arc::new(HttpProviderApi::new()),
        mailer,
        cra,
        today: Some(today()),
    }
}

pub(crate) fn cookies(resp: &actix_web::dev::ServiceResponse) -> Vec<Cookie<'static>> {
    resp.response().cookies().map(|c| c.into_owned()).collect()
}

pub(crate) fn cookie_named(
    resp: &actix_web::dev::ServiceResponse,
    name: &str,
) -> Option<Cookie<'static>> {
    cookies(resp).into_iter().find(|c| c.name() == name)
}

pub(crate) fn location(resp: &actix_web::dev::ServiceResponse) -> Option<String> {
    resp.headers()
        .get("Location")
        .map(|h| h.to_str().unwrap().to_string())
}

pub(crate) fn query_param(url: &str, key: &str) -> Option<String> {
    let q = url.split_once('?')?.1;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

pub(crate) fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 3 <= b.len() {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(if b[i] == b'+' { b' ' } else { b[i] });
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

macro_rules! app {
    ($ctx:expr) => {
        actix_web::test::init_service(
            actix_web::App::new()
                .app_data(actix_web::web::Data::new($ctx))
                .configure($crate::accounts::routes::configure),
        )
        .await
    };
}

macro_rules! account_json {
    ($app:expr, $session:expr) => {{
        let resp = actix_web::test::call_service(
            &$app,
            actix_web::test::TestRequest::get()
                .uri("/account")
                .cookie($session)
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::OK);
        let v: serde_json::Value = actix_web::test::read_body_json(resp).await;
        v
    }};
}

macro_rules! account_status {
    ($app:expr, $session:expr) => {{
        actix_web::test::call_service(
            &$app,
            actix_web::test::TestRequest::get()
                .uri("/account")
                .cookie($session)
                .to_request(),
        )
        .await
        .status()
    }};
}

/// Start a sign-in: returns (state, sign-in cookie).
macro_rules! start {
    ($app:expr, $provider:expr, $session:expr, $link:expr) => {{
        let mut uri = format!("/auth/{}/start", $provider);
        if let Some(l) = $link {
            uri = format!("{uri}?link={l}");
        }
        let mut req = actix_web::test::TestRequest::get().uri(&uri);
        if let Some(c) = $session {
            req = req.cookie(c);
        }
        let resp = actix_web::test::call_service(&$app, req.to_request()).await;
        assert_eq!(
            resp.status(),
            actix_web::http::StatusCode::FOUND,
            "start {}",
            $provider
        );
        let loc = $crate::accounts::tests::location(&resp).unwrap();
        (
            $crate::accounts::tests::query_param(&loc, "state").unwrap(),
            $crate::accounts::tests::cookie_named(&resp, $crate::accounts::routes::SIGNIN_COOKIE)
                .unwrap(),
        )
    }};
}

/// Run the callback for `code`, from the browser holding `signin` (and optionally a session).
macro_rules! callback {
    ($app:expr, $provider:expr, $code:expr, $state:expr, $signin:expr, $session:expr) => {{
        let mut req = actix_web::test::TestRequest::get().uri(&format!(
            "/auth/{}/callback?code={}&state={}",
            $provider, $code, $state
        ));
        if let Some(c) = $signin {
            req = req.cookie(c);
        }
        if let Some(c) = $session {
            req = req.cookie(c);
        }
        actix_web::test::call_service(&$app, req.to_request()).await
    }};
}

/// A full sign-in; returns the session cookie.
macro_rules! sign_in {
    ($app:expr, $provider:expr, $code:expr) => {{
        let (state, signin) = start!(
            $app,
            $provider,
            None::<actix_web::cookie::Cookie<'static>>,
            None::<String>
        );
        let resp = callback!(
            $app,
            $provider,
            $code,
            state,
            Some(signin),
            None::<actix_web::cookie::Cookie<'static>>
        );
        assert_eq!(
            resp.status(),
            actix_web::http::StatusCode::FOUND,
            "callback {}",
            $provider
        );
        $crate::accounts::tests::cookie_named(&resp, $crate::accounts::routes::SESSION_COOKIE)
            .expect("a session cookie")
    }};
}

/// Link an identity to the signed-in session's account.
macro_rules! link_id {
    ($app:expr, $provider:expr, $code:expr, $session:expr) => {{
        let nonce = account_json!($app, $session.clone())["link_nonce"]
            .as_str()
            .unwrap()
            .to_string();
        let (state, signin) = start!($app, $provider, Some($session.clone()), Some(nonce));
        callback!(
            $app,
            $provider,
            $code,
            state,
            Some(signin),
            Some($session.clone())
        )
    }};
}

#[allow(unused_imports)]
pub(crate) use {account_json, account_status, app, callback, link_id, sign_in, start};

async fn session_count(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM account_sessions")
        .fetch_one(pool)
        .await
        .unwrap()
}

// ----------------------------------------------------------------- linking

/// Linking adds a door, never authority: an account's CRA standing is the same after every
/// provider is linked, including another member's chat account.
#[tokio::test]
async fn linking_every_provider_never_changes_standing() {
    let pool = pool().await;
    let cra = CraHandle::new();
    activate(&cra, &eustress_files());
    let mut m = Mocks::new().await;
    m.github("gh1", 1001, "Simbuilder", None).await;
    m.gitlab("gl1", 2001, "simbuilder-on-gitlab").await;
    m.discord("dc1", "100000000000000102", None, false).await;
    m.slack("sl1", "T1000000000", "U1000000102").await;
    m.google("go1", "g-1", "mckale@gmail.example", true).await;
    let app = app!(accounts_ctx(
        pool.clone(),
        cra.clone(),
        &m.url(),
        Arc::new(RecordingMailer::default())
    ));

    let s = sign_in!(app, "github", "gh1");
    let before = account_json!(app, s.clone());
    assert_eq!(before["member_id"], "mckale");
    let standing = before["standing"].clone();
    assert_eq!(standing["rank"], 80, "Lead Maintainer");
    for (p, code) in [
        ("gitlab", "gl1"),
        ("discord", "dc1"),
        ("slack", "sl1"),
        ("google", "go1"),
    ] {
        let r = link_id!(app, p, code, s);
        assert_eq!(r.status(), StatusCode::FOUND, "link {p}");
        let now = account_json!(app, s.clone());
        assert_eq!(now["member_id"], "mckale", "after linking {p}");
        assert_eq!(now["standing"], standing, "after linking {p}");
    }
    let now = account_json!(app, s.clone());
    assert_eq!(now["identities"].as_array().unwrap().len(), 5);

    // Another person: Cy (no chat ids in the CRA) links Ada's Discord account. Cy stays Cy and
    // gets none of Ada's Maintainer authority.
    m.github("gh-cy", 1005, "cy-example", None).await;
    m.discord("dc-ada", "100000000000000103", None, false).await;
    let cy = sign_in!(app, "github", "gh-cy");
    let cy_before = account_json!(app, cy.clone());
    assert_eq!(cy_before["member_id"], "cy-example");
    assert_eq!(
        link_id!(app, "discord", "dc-ada", cy).status(),
        StatusCode::FOUND
    );
    let cy_after = account_json!(app, cy.clone());
    assert_eq!(cy_after["member_id"], "cy-example");
    assert_eq!(cy_after["standing"], cy_before["standing"]);

    // And no identity column can carry authority.
    let cols: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('identities')")
        .fetch_all(&pool)
        .await
        .unwrap();
    for c in &cols {
        assert!(
            ![
                "rank",
                "role",
                "roles",
                "permission",
                "permissions",
                "grant",
                "tier"
            ]
            .contains(&c.as_str()),
            "{c}"
        );
    }
}

#[tokio::test]
async fn store_linking_follows_the_rule() {
    let pool = pool().await;
    let p = |provider, subject: &str, email: Option<&str>, verified: bool| ProviderProfile {
        provider,
        subject: subject.into(),
        handle: Some(subject.into()),
        display_name: None,
        email: email.map(str::to_string),
        email_verified: verified,
        login: None,
        team: None,
    };
    // A Google account with a verified address.
    let a = store::complete_sign_in(
        &pool,
        &p(Provider::Google, "g-a", Some("Jo@X.io"), true),
        None,
        1,
    )
    .await
    .unwrap();
    assert_eq!(a.decision, Decision::CreateAccount);
    // Discord with the same address, NOT marked verified: a new account.
    let d = store::complete_sign_in(
        &pool,
        &p(Provider::Discord, "1", Some("jo@x.io"), false),
        None,
        2,
    )
    .await
    .unwrap();
    assert_eq!(d.decision, Decision::CreateAccount);
    assert_ne!(d.account_id, a.account_id);
    // Slack with the address marked verified: attaches to the one account with it.
    let s = store::complete_sign_in(
        &pool,
        &p(Provider::Slack, "T1:U1", Some("jo@x.io"), true),
        None,
        3,
    )
    .await
    .unwrap();
    assert_eq!(
        s.decision,
        Decision::AttachTo {
            account: a.account_id.clone(),
            auto: true
        }
    );
    // The same identity again signs in to its account.
    let again = store::complete_sign_in(&pool, &p(Provider::Discord, "1", None, false), None, 4)
        .await
        .unwrap();
    assert_eq!(again.decision, Decision::SignIn(d.account_id.clone()));
    // An explicit link of an identity owned by another account is refused.
    let r = store::complete_sign_in(
        &pool,
        &p(Provider::Discord, "1", None, false),
        Some(&a.account_id),
        5,
    )
    .await;
    assert!(
        matches!(r, Err(AccountsError::Refused(Refusal::OtherAccount))),
        "{r:?}"
    );
    // A second Slack identity on one account is refused.
    let r = store::complete_sign_in(
        &pool,
        &p(Provider::Slack, "T1:U2", None, false),
        Some(&a.account_id),
        6,
    )
    .await;
    assert!(
        matches!(r, Err(AccountsError::Refused(Refusal::SecondOfProvider))),
        "{r:?}"
    );
    // Refusals are recorded for the member.
    let refused: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM identity_events WHERE kind = 'refused' AND account_id = ?",
    )
    .bind(&a.account_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(refused, 2);
    // A verified address shared by two accounts is ambiguous: a new account.
    store::complete_sign_in(
        &pool,
        &p(Provider::Email, "jo@x.io", Some("jo@x.io"), true),
        Some(&d.account_id),
        7,
    )
    .await
    .unwrap();
    let g = store::complete_sign_in(
        &pool,
        &p(Provider::Google, "g-b", Some("jo@x.io"), true),
        None,
        8,
    )
    .await
    .unwrap();
    assert_eq!(g.decision, Decision::CreateAccount);
}

#[tokio::test]
async fn the_last_identity_cannot_be_unlinked() {
    let pool = pool().await;
    let mut m = Mocks::new().await;
    m.github("gh1", 1001, "alice", None).await;
    m.discord("dc1", "42", None, false).await;
    let app = app!(accounts_ctx(
        pool.clone(),
        CraHandle::new(),
        &m.url(),
        Arc::new(RecordingMailer::default())
    ));
    let s = sign_in!(app, "github", "gh1");
    let ids = account_json!(app, s.clone())["identities"].clone();
    let gh = ids[0]["id"].as_str().unwrap().to_string();
    let unlink = |id: &str| {
        atest::TestRequest::post()
            .uri(&format!("/account/identities/{id}/unlink"))
            .insert_header(("Origin", PUBLIC))
            .cookie(s.clone())
            .to_request()
    };

    let resp = atest::call_service(&app, unlink(&gh)).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let v: Value = atest::read_body_json(resp).await;
    assert_eq!(v["error"], "last");

    assert_eq!(
        link_id!(app, "discord", "dc1", s).status(),
        StatusCode::FOUND
    );
    let ids = account_json!(app, s.clone())["identities"].clone();
    let dc = ids
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["provider"] == "discord")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        atest::call_service(&app, unlink(&dc)).await.status(),
        StatusCode::OK
    );
    // Back to one: refused again.
    assert_eq!(
        atest::call_service(&app, unlink(&gh)).await.status(),
        StatusCode::CONFLICT
    );
    assert!(
        login_is_verified(&pool, crate::foundry::FoundryProvider::GitHub, "alice")
            .await
            .unwrap()
    );
}

// ---------------------------------------------------------------- sign-in

#[tokio::test]
async fn a_callback_with_a_wrong_or_missing_state_is_refused() {
    let pool = pool().await;
    let mut m = Mocks::new().await;
    m.github("gh1", 1001, "alice", None).await;
    let app = app!(accounts_ctx(
        pool.clone(),
        CraHandle::new(),
        &m.url(),
        Arc::new(RecordingMailer::default())
    ));
    let none = None::<Cookie<'static>>;

    let (state, signin) = start!(app, "github", none.clone(), None::<String>);
    // Missing state.
    let r = atest::call_service(
        &app,
        atest::TestRequest::get()
            .uri("/auth/github/callback?code=gh1")
            .cookie(signin.clone())
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    // Wrong state (right shape).
    let wrong = rules::random_token().unwrap();
    assert_eq!(
        callback!(
            app,
            "github",
            "gh1",
            wrong,
            Some(signin.clone()),
            none.clone()
        )
        .status(),
        StatusCode::BAD_REQUEST
    );
    // Right state, no sign-in cookie: not the browser that started it.
    assert_eq!(
        callback!(
            app,
            "github",
            "gh1",
            state,
            None::<Cookie<'static>>,
            none.clone()
        )
        .status(),
        StatusCode::BAD_REQUEST
    );
    // Right state from ANOTHER browser (an attacker's started sign-in pasted into a victim's).
    let (_, other_browser) = start!(app, "github", none.clone(), None::<String>);
    assert_eq!(
        callback!(
            app,
            "github",
            "gh1",
            state,
            Some(other_browser),
            none.clone()
        )
        .status(),
        StatusCode::BAD_REQUEST
    );
    // State for another provider.
    let (gl_state, gl_signin) = start!(app, "gitlab", none.clone(), None::<String>);
    assert_eq!(
        callback!(
            app,
            "github",
            "gh1",
            gl_state,
            Some(gl_signin),
            none.clone()
        )
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(session_count(&pool).await, 0);

    // The real one works once, then never again.
    let ok = callback!(
        app,
        "github",
        "gh1",
        state,
        Some(signin.clone()),
        none.clone()
    );
    assert_eq!(ok.status(), StatusCode::FOUND);
    assert_eq!(session_count(&pool).await, 1);
    assert_eq!(
        callback!(app, "github", "gh1", state, Some(signin), none.clone()).status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(session_count(&pool).await, 1);
}

#[tokio::test]
async fn a_disabled_provider_is_refused() {
    let pool = pool().await;
    let mut m = Mocks::new().await;
    m.discord("dc1", "42", None, false).await;
    let mut ctx = accounts_ctx(
        pool.clone(),
        CraHandle::new(),
        &m.url(),
        Arc::new(RecordingMailer::default()),
    );
    // Discord has a client id but no secret: not enabled.
    let lookup = |k: &str| (k == "MSYNC_OAUTH_DISCORD_CLIENT_ID").then(|| "id".to_string());
    ctx.providers.retain(|p, _| *p != Provider::Discord);
    if let Some(c) = ProviderConfig::from_lookup(Provider::Discord, lookup) {
        ctx.providers.insert(
            Provider::Discord,
            ProviderConfig {
                authorize_url: format!("{}/x", m.url()),
                ..c
            },
        );
    }
    let app = app!(ctx);
    let r = atest::call_service(
        &app,
        atest::TestRequest::get()
            .uri("/auth/discord/start")
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let state = rules::random_token().unwrap();
    let r = atest::call_service(
        &app,
        atest::TestRequest::get()
            .uri(&format!("/auth/discord/callback?code=dc1&state={state}"))
            .cookie(Cookie::new(
                routes::SIGNIN_COOKIE,
                rules::random_token().unwrap(),
            ))
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let v: Value = atest::read_body_json(r).await;
    assert_eq!(v["error"], "provider_disabled");
    // Unknown providers too.
    let r = atest::call_service(
        &app,
        atest::TestRequest::get()
            .uri("/auth/myspace/start")
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert_eq!(session_count(&pool).await, 0);
}

#[tokio::test]
async fn a_provider_error_creates_no_session() {
    let pool = pool().await;
    let mut m = Mocks::new().await;
    m.github("gh1", 1001, "alice", None).await;
    let app = app!(accounts_ctx(
        pool.clone(),
        CraHandle::new(),
        &m.url(),
        Arc::new(RecordingMailer::default())
    ));
    let none = None::<Cookie<'static>>;

    // The provider redirected back with an error (the member declined), even with a code.
    let (state, signin) = start!(app, "github", none.clone(), None::<String>);
    let r = atest::call_service(
        &app,
        atest::TestRequest::get()
            .uri(&format!(
                "/auth/github/callback?error=access_denied&code=gh1&state={state}"
            ))
            .cookie(signin)
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert!(cookie_named(&r, routes::SESSION_COOKIE).is_none());

    // The code exchange fails at the provider (the mock knows no such code).
    let (state, signin) = start!(app, "github", none.clone(), None::<String>);
    let r = callback!(
        app,
        "github",
        "not-a-code",
        state,
        Some(signin),
        none.clone()
    );
    assert_eq!(r.status(), StatusCode::BAD_GATEWAY);
    assert!(cookie_named(&r, routes::SESSION_COOKIE).is_none());
    assert_eq!(session_count(&pool).await, 0);
    let accounts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(accounts, 0);
}

#[tokio::test]
async fn sessions_are_revoked_on_the_next_request() {
    let pool = pool().await;
    let mut m = Mocks::new().await;
    m.github("gh1", 1001, "alice", None).await;
    m.github("gh2", 1001, "alice", None).await;
    m.github("gh3", 1001, "alice", None).await;
    let app = app!(accounts_ctx(
        pool.clone(),
        CraHandle::new(),
        &m.url(),
        Arc::new(RecordingMailer::default())
    ));
    let logout = |c: Cookie<'static>, all: bool| {
        atest::TestRequest::post()
            .uri(if all {
                "/auth/logout?all=true"
            } else {
                "/auth/logout"
            })
            .insert_header(("Origin", PUBLIC))
            .cookie(c)
            .to_request()
    };

    // One session.
    let s1 = sign_in!(app, "github", "gh1");
    assert_eq!(account_status!(app, s1.clone()), StatusCode::OK);
    assert_eq!(
        atest::call_service(&app, logout(s1.clone(), false))
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        account_status!(app, s1.clone()),
        StatusCode::UNAUTHORIZED,
        "revoked on the next request"
    );

    // All sessions.
    let s2 = sign_in!(app, "github", "gh2");
    let s3 = sign_in!(app, "github", "gh3");
    assert_eq!(
        atest::call_service(&app, logout(s2.clone(), true))
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(account_status!(app, s2.clone()), StatusCode::UNAUTHORIZED);
    assert_eq!(account_status!(app, s3.clone()), StatusCode::UNAUTHORIZED);

    // Suspension ends every session.
    m.github("gh4", 1001, "alice", None).await;
    let s4 = sign_in!(app, "github", "gh4");
    let account: String = sqlx::query_scalar("SELECT id FROM accounts")
        .fetch_one(&pool)
        .await
        .unwrap();
    store::suspend_account(&pool, &account, "test", chrono::Utc::now().timestamp())
        .await
        .unwrap();
    assert_eq!(account_status!(app, s4.clone()), StatusCode::UNAUTHORIZED);
    // And a suspended account cannot sign in again.
    m.github("gh5", 1001, "alice", None).await;
    let (state, signin) = start!(app, "github", None::<Cookie<'static>>, None::<String>);
    let r = callback!(
        app,
        "github",
        "gh5",
        state,
        Some(signin),
        None::<Cookie<'static>>
    );
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert!(cookie_named(&r, routes::SESSION_COOKIE).is_none());
}

#[tokio::test]
async fn the_session_cookie_is_secure_httponly_and_samesite_lax() {
    let pool = pool().await;
    let mut m = Mocks::new().await;
    m.github("gh1", 1001, "alice", None).await;
    let app = app!(accounts_ctx(
        pool.clone(),
        CraHandle::new(),
        &m.url(),
        Arc::new(RecordingMailer::default())
    ));
    let (state, signin) = start!(app, "github", None::<Cookie<'static>>, None::<String>);
    let r = callback!(
        app,
        "github",
        "gh1",
        state,
        Some(signin),
        None::<Cookie<'static>>
    );
    let header: Vec<String> = r
        .headers()
        .get_all("Set-Cookie")
        .map(|h| h.to_str().unwrap().to_string())
        .filter(|h| h.starts_with(&format!("{}=", routes::SESSION_COOKIE)))
        .collect();
    assert_eq!(header.len(), 1, "{header:?}");
    let h = &header[0];
    for flag in ["Secure", "HttpOnly", "SameSite=Lax", "Path=/", "Max-Age="] {
        assert!(h.contains(flag), "{flag} missing from {h}");
    }
    // 256 random bits; only the hash is stored.
    let token = h
        .split(';')
        .next()
        .unwrap()
        .split_once('=')
        .unwrap()
        .1
        .to_string();
    assert_eq!(token.len(), 43);
    let stored: String = sqlx::query_scalar("SELECT id FROM account_sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, rules::token_hash(&token));
    assert_ne!(stored, token);
}

#[tokio::test]
async fn a_cross_origin_post_to_a_cookie_route_is_refused() {
    let pool = pool().await;
    let mut m = Mocks::new().await;
    m.github("gh1", 1001, "alice", None).await;
    let app = app!(accounts_ctx(
        pool.clone(),
        CraHandle::new(),
        &m.url(),
        Arc::new(RecordingMailer::default())
    ));
    let s = sign_in!(app, "github", "gh1");
    for origin in [
        Some("https://evil.example"),
        Some("https://msync.example.evil.example"),
        Some("null"),
        None,
    ] {
        let mut req = atest::TestRequest::post()
            .uri("/auth/logout")
            .cookie(s.clone());
        if let Some(o) = origin {
            req = req.insert_header(("Origin", o));
        }
        let r = atest::call_service(&app, req.to_request()).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "{origin:?}");
    }
    assert_eq!(
        account_status!(app, s.clone()),
        StatusCode::OK,
        "the session survived"
    );
    let id = account_json!(app, s.clone())["identities"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = atest::call_service(
        &app,
        atest::TestRequest::post()
            .uri(&format!("/account/identities/{id}/unlink"))
            .insert_header(("Origin", "https://evil.example"))
            .cookie(s.clone())
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------- email links

#[tokio::test]
async fn email_links_are_single_use_expire_and_are_rate_limited() {
    let pool = pool().await;
    let mailer = Arc::new(RecordingMailer::default());
    let app = app!(accounts_ctx(
        pool.clone(),
        CraHandle::new(),
        "http://unused",
        mailer.clone()
    ));
    let ask = |email: &str| {
        atest::TestRequest::post()
            .uri("/auth/email/start")
            .insert_header(("Origin", PUBLIC))
            .peer_addr("10.0.0.1:1".parse().unwrap())
            .set_json(json!({"email": email}))
            .to_request()
    };
    assert_eq!(
        atest::call_service(&app, ask("  Jo@X.io ")).await.status(),
        StatusCode::OK
    );
    let (to, url) = mailer.sent.lock().unwrap()[0].clone();
    assert_eq!(to, "jo@x.io");
    let token = query_param(&url, "token").unwrap();
    assert!(url.starts_with("https://msync.example/auth/email/verify?token="));
    let stored: String = sqlx::query_scalar("SELECT token_hash FROM email_tokens")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, rules::token_hash(&token), "stored hashed");

    // Looking does not use it.
    let look = atest::call_service(
        &app,
        atest::TestRequest::get()
            .uri(&format!("/auth/email/verify?token={token}"))
            .to_request(),
    )
    .await;
    assert_eq!(look.status(), StatusCode::OK);
    let use_it = || {
        atest::TestRequest::post()
            .uri("/auth/email/verify")
            .insert_header(("Origin", PUBLIC))
            .set_form([("token", token.clone())])
            .to_request()
    };
    let r = atest::call_service(&app, use_it()).await;
    assert_eq!(r.status(), StatusCode::FOUND);
    assert!(cookie_named(&r, routes::SESSION_COOKIE).is_some());
    let r = atest::call_service(&app, use_it()).await;
    assert_eq!(r.status(), StatusCode::GONE, "single use");

    // An expired token is refused.
    sqlx::query("UPDATE email_tokens SET used_at = NULL, expires_at = 0")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        atest::call_service(&app, use_it()).await.status(),
        StatusCode::GONE
    );

    // Three per address per 15 minutes; the fourth is refused and nothing is sent.
    assert_eq!(
        atest::call_service(&app, ask("jo@x.io")).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        atest::call_service(&app, ask("jo@x.io")).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        atest::call_service(&app, ask("jo@x.io")).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(mailer.sent.lock().unwrap().len(), 3);
    // Ten per IP per hour across addresses.
    for i in 0..6 {
        assert_eq!(
            atest::call_service(&app, ask(&format!("p{i}@x.io")))
                .await
                .status(),
            StatusCode::OK,
            "{i}"
        );
    }
    assert_eq!(
        atest::call_service(&app, ask("p9@x.io")).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        atest::call_service(&app, ask("not an address"))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn the_default_mailer_sends_nothing() {
    assert_eq!(
        NoMailer
            .send_sign_in_link("jo@x.io", "https://msync.example/auth/email/verify?token=x")
            .await,
        Ok(())
    );
}

// --------------------------------------------------- verification flows outward

fn d(id: &str) -> ChatActor {
    ChatActor::discord(id, Some(false))
}

#[tokio::test]
async fn oauth_proof_of_both_sides_verifies_a_chat_binding_for_payout_setup() {
    let pool = pool().await;
    let handle = CraHandle::new();
    let dir: Arc<dyn crate::auth::Directory> = Arc::new(crate::auth::CraDirectory::new(
        crate::auth::EnvDirectory::default(),
        handle.clone(),
    ));
    let env = crate::chat::tests::env_from(pool.clone(), handle.clone(), dir);
    // The old way: someone typed `link github:alice` from Discord 42. Unverified.
    crate::foundry::identity::resolve_contributor(
        &pool,
        crate::foundry::FoundryProvider::GitHub,
        "github.com",
        "alice",
    )
    .await
    .unwrap();
    assert!(identity::bind(&pool, &d("42"), "alice", false, "link")
        .await
        .unwrap());
    let r = commands::payout_setup(&env.ctx, &d("42")).await;
    assert_eq!(
        r.text,
        commands::unverified_payout_text(ChatPlatform::Discord)
    );

    let mut m = Mocks::new().await;
    m.discord("dc1", "42", None, false).await;
    m.github("gh1", 1001, "alice", None).await;
    let app = app!(accounts_ctx(
        pool.clone(),
        handle.clone(),
        &m.url(),
        Arc::new(RecordingMailer::default())
    ));

    // Proving only the Discord side verifies nothing: the login was typed, not proven.
    let s = sign_in!(app, "discord", "dc1");
    let b = identity::binding_for(&pool, &d("42"))
        .await
        .unwrap()
        .unwrap();
    assert!(!b.verified);
    assert!(
        !login_is_verified(&pool, crate::foundry::FoundryProvider::GitHub, "alice")
            .await
            .unwrap()
    );
    assert_eq!(
        commands::payout_setup(&env.ctx, &d("42")).await.text,
        commands::unverified_payout_text(ChatPlatform::Discord)
    );

    // Linking the GitHub login on the same account proves both sides.
    assert_eq!(
        link_id!(app, "github", "gh1", s).status(),
        StatusCode::FOUND
    );
    let b = identity::binding_for(&pool, &d("42"))
        .await
        .unwrap()
        .unwrap();
    assert!(b.verified, "the chat binding is now verified");
    assert!(
        login_is_verified(&pool, crate::foundry::FoundryProvider::GitHub, "ALICE")
            .await
            .unwrap()
    );
    assert!(
        !login_is_verified(&pool, crate::foundry::FoundryProvider::GitLab, "alice")
            .await
            .unwrap()
    );
    let r = commands::payout_setup(&env.ctx, &d("42")).await;
    assert!(r.text.contains("connect.stripe.test"), "{}", r.text);
    assert!(r.private);
}

#[tokio::test]
async fn payout_setup_accepts_a_binding_made_after_the_oauth_proof() {
    let pool = pool().await;
    let handle = CraHandle::new();
    let dir: Arc<dyn crate::auth::Directory> = Arc::new(crate::auth::CraDirectory::new(
        crate::auth::EnvDirectory::default(),
        handle.clone(),
    ));
    let env = crate::chat::tests::env_from(pool.clone(), handle.clone(), dir);
    let mut m = Mocks::new().await;
    m.github("gh1", 1001, "alice", None).await;
    m.discord("dc1", "42", None, false).await;
    let app = app!(accounts_ctx(
        pool.clone(),
        handle.clone(),
        &m.url(),
        Arc::new(RecordingMailer::default())
    ));
    let s = sign_in!(app, "github", "gh1");
    assert_eq!(
        link_id!(app, "discord", "dc1", s).status(),
        StatusCode::FOUND
    );
    // The chat binding is made afterwards (so nothing marked it), as unverified.
    crate::foundry::identity::resolve_contributor(
        &pool,
        crate::foundry::FoundryProvider::GitHub,
        "github.com",
        "alice",
    )
    .await
    .unwrap();
    assert!(identity::bind(&pool, &d("42"), "alice", false, "link")
        .await
        .unwrap());
    assert!(
        !identity::binding_for(&pool, &d("42"))
            .await
            .unwrap()
            .unwrap()
            .verified
    );
    assert!(
        chat_binding_is_verified(&pool, ChatPlatform::Discord, "", "42", "alice")
            .await
            .unwrap()
    );
    let r = commands::payout_setup(&env.ctx, &d("42")).await;
    assert!(r.text.contains("connect.stripe.test"), "{}", r.text);
    // Another Discord user bound to alice by typing is not proven.
    assert!(
        !chat_binding_is_verified(&pool, ChatPlatform::Discord, "", "43", "alice")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn member_resolution_uses_proven_identities_only() {
    let pool = pool().await;
    let cra = CraHandle::new();
    activate(&cra, &eustress_files());
    let snap = cra.active().unwrap().snapshot.clone();
    // An email that happens to match nothing, and a Slack id in ANOTHER workspace: nobody.
    let wrong_team = store::complete_sign_in(
        &pool,
        &ProviderProfile {
            provider: Provider::Slack,
            subject: "T9999999999:U1000000102".into(),
            handle: None,
            display_name: None,
            email: None,
            email_verified: false,
            login: None,
            team: Some("T9999999999".into()),
        },
        None,
        1,
    )
    .await
    .unwrap();
    assert_eq!(
        member_for_account(&pool, &snap, &wrong_team.account_id)
            .await
            .unwrap(),
        None
    );
    let right_team = store::complete_sign_in(
        &pool,
        &ProviderProfile {
            provider: Provider::Slack,
            subject: "T1000000000:U1000000102".into(),
            handle: None,
            display_name: None,
            email: None,
            email_verified: false,
            login: None,
            team: Some("T1000000000".into()),
        },
        None,
        2,
    )
    .await
    .unwrap();
    assert_eq!(
        member_for_account(&pool, &snap, &right_team.account_id)
            .await
            .unwrap()
            .as_deref(),
        Some("mckale")
    );
}
