//! Slack adapter: signature verification, team check, ephemeral replies, admin detection
//! through the Web API trait, and user group application.

use std::sync::{Arc, Mutex};

use actix_web::{http::StatusCode, test as atest, App};
use async_trait::async_trait;

use super::*;
use crate::chat::tests::{activate, env_with, eustress_files, Env};

const SECRET: &str = "8f742231b10e8888abcd99yyyzzz85a5";
const NOW: i64 = 1_763_600_000;

fn ts(t: i64) -> String {
    t.to_string()
}

// ---------------------------------------------------------------- signature

#[test]
fn a_valid_signature_is_accepted() {
    let body = b"command=%2Fmsync-stats&user_id=U1&team_id=T1";
    let sig = slack_v0_signature(SECRET, &ts(NOW), body);
    assert!(verify_slack_signature(SECRET, Some(&ts(NOW)), Some(&sig), body, NOW));
    // Inside the window on either side.
    assert!(verify_slack_signature(SECRET, Some(&ts(NOW)), Some(&sig), body, NOW + 300));
    assert!(verify_slack_signature(SECRET, Some(&ts(NOW)), Some(&sig), body, NOW - 300));
}

/// The signature Slack documents for its example request, so the base string format
/// (`v0:<timestamp>:<body>`) is the real one and not just consistent with itself.
#[test]
fn slacks_documented_example_verifies() {
    let body = b"token=xyzz0WbapA4vBCDEFasx0q6G&team_id=T1DC2JH3J&team_domain=testteamnow&channel_id=G8PSS9T3V&channel_name=foobar&user_id=U2CERLKJA&user_name=roadrunner&command=%2Fwebhook-collect&text=&response_url=https%3A%2F%2Fhooks.slack.com%2Fcommands%2FT1DC2JH3J%2F397700885554%2F96rGlfmibIGlgcZRskXaIFfN&trigger_id=398738663015.47445629121.803a0bc887a14d10d2c447fce8b6703c";
    let sig = "v0=a2114d57b48eac39b9ad189dd8316235a7b4a8d21a10bd27519666489c69b503";
    assert!(verify_slack_signature(SECRET, Some("1531420618"), Some(sig), body, 1531420618));
    assert_eq!(slack_v0_signature(SECRET, "1531420618", body), sig);
}

#[test]
fn wrong_secret_tampered_body_stale_and_missing_headers_are_rejected() {
    let body = b"command=%2Fmsync-stats&user_id=U1&team_id=T1";
    let t = ts(NOW);
    let sig = slack_v0_signature(SECRET, &t, body);
    // Wrong secret.
    assert!(!verify_slack_signature("another-secret", Some(&t), Some(&sig), body, NOW));
    // Tampered body.
    assert!(!verify_slack_signature(SECRET, Some(&t), Some(&sig), b"command=%2Fmsync-stats&user_id=U2&team_id=T1", NOW));
    // Stale (and too far in the future).
    assert!(!verify_slack_signature(SECRET, Some(&t), Some(&sig), body, NOW + 301));
    assert!(!verify_slack_signature(SECRET, Some(&t), Some(&sig), body, NOW - 301));
    // A stale request re-signed with a fresh timestamp header is a different signature.
    assert!(!verify_slack_signature(SECRET, Some(&ts(NOW + 400)), Some(&sig), body, NOW + 400));
    // Missing or malformed headers.
    assert!(!verify_slack_signature(SECRET, None, Some(&sig), body, NOW));
    assert!(!verify_slack_signature(SECRET, Some(&t), None, body, NOW));
    assert!(!verify_slack_signature(SECRET, Some("soon"), Some(&sig), body, NOW));
    assert!(!verify_slack_signature(SECRET, Some(&t), Some(sig.trim_start_matches("v0=")), body, NOW));
    assert!(!verify_slack_signature(SECRET, Some(&t), Some("v1=00"), body, NOW));
    assert!(!verify_slack_signature(SECRET, Some(&t), Some("v0=zz"), body, NOW));
}

#[test]
fn an_empty_configured_secret_rejects_everything() {
    let body = b"command=%2Fmsync-stats&user_id=U1&team_id=T1";
    let t = ts(NOW);
    // Even a request correctly "signed" with the empty key.
    let sig = slack_v0_signature("", &t, body);
    assert!(!verify_slack_signature("", Some(&t), Some(&sig), body, NOW));
}

// --------------------------------------------------------------- Web API mock

#[derive(Default)]
pub(crate) struct MockSlack {
    pub admin: Option<bool>,
    pub admin_calls: Mutex<Vec<String>>,
    pub updates: Mutex<Vec<(String, Vec<String>)>>,
}

#[async_trait]
impl SlackApi for MockSlack {
    async fn is_admin(&self, user_id: &str) -> Option<bool> {
        self.admin_calls.lock().unwrap().push(user_id.to_string());
        self.admin
    }
    async fn set_usergroup_users(&self, usergroup_id: &str, user_ids: &[String]) -> Result<(), String> {
        self.updates.lock().unwrap().push((usergroup_id.to_string(), user_ids.to_vec()));
        Ok(())
    }
}

fn state(e: &Env, secret: Option<&str>, team: Option<&str>, api: Arc<MockSlack>) -> SlackState {
    SlackState {
        chat: e.ctx.clone(),
        signing_secret: secret.map(String::from),
        team_id: team.map(String::from),
        api,
    }
}

fn form(command: &str, text: &str, user: &str, team: &str) -> Vec<u8> {
    format!(
        "token=x&team_id={team}&team_domain=example&channel_id=C1&user_id={user}&user_name=u&command={}&text={}&response_url=https%3A%2F%2Fhooks.slack.com%2Fcommands%2Fx",
        command.replace('/', "%2F"),
        text.replace(' ', "+").replace(':', "%3A")
    )
    .into_bytes()
}

async fn send(st: &SlackState, body: &[u8]) -> SlackOutcome {
    let t = ts(NOW);
    let sig = slack_v0_signature(SECRET, &t, body);
    handle_command(st, Some(&t), Some(&sig), body, NOW).await
}

fn json_text(o: &SlackOutcome) -> (String, String) {
    let b = o.body.as_ref().expect("a 200 has a body");
    (b["response_type"].as_str().unwrap().to_string(), b["text"].as_str().unwrap().to_string())
}

// ---------------------------------------------------------------- the route

#[tokio::test]
async fn requests_are_verified_before_anything_runs() {
    let e = env_with(false).await;
    let api = Arc::new(MockSlack::default());
    let st = state(&e, Some(SECRET), Some("T0TEAM0001"), api.clone());
    let body = form("/msync-link", "alice", "U0ALICE0001", "T0TEAM0001");

    // Bad signature: 401, and the link never happened.
    let t = ts(NOW);
    let out = handle_command(&st, Some(&t), Some(&slack_v0_signature("wrong", &t, &body)), &body, NOW).await;
    assert_eq!(out.status, StatusCode::UNAUTHORIZED);
    let out = handle_command(&st, None, None, &body, NOW).await;
    assert_eq!(out.status, StatusCode::UNAUTHORIZED);
    let stale = ts(NOW - 301);
    let out = handle_command(&st, Some(&stale), Some(&slack_v0_signature(SECRET, &stale, &body)), &body, NOW).await;
    assert_eq!(out.status, StatusCode::UNAUTHORIZED);
    // No secret configured: 401 even with a valid-looking request.
    for secret in [None, Some("")] {
        let off = state(&e, secret, Some("T0TEAM0001"), api.clone());
        assert!(!off.enabled());
        assert_eq!(send(&off, &body).await.status, StatusCode::UNAUTHORIZED);
    }
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_identities").fetch_one(&e.ctx.pool).await.unwrap();
    assert_eq!(n, 0, "a rejected request changed state");

    // Valid: runs.
    let out = send(&st, &body).await;
    assert_eq!(out.status, StatusCode::OK);
    let (kind, text) = json_text(&out);
    assert_eq!(kind, "ephemeral");
    assert!(text.contains("Linked your Slack account"), "{text}");
}

#[tokio::test]
async fn a_foreign_team_is_rejected() {
    let e = env_with(false).await;
    let st = state(&e, Some(SECRET), Some("T0TEAM0001"), Arc::new(MockSlack::default()));
    let out = send(&st, &form("/msync-leaderboard", "", "U0EVE00001", "T0EVIL0001")).await;
    assert_eq!(out.status, StatusCode::FORBIDDEN);
    assert!(out.body.is_none());
    // No team configured and no CRA: every team is foreign.
    let none = state(&e, Some(SECRET), None, Arc::new(MockSlack::default()));
    assert_eq!(send(&none, &form("/msync-leaderboard", "", "U0EVE00001", "T0TEAM0001")).await.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_cra_listed_team_is_accepted_and_others_are_not() {
    let e = env_with(false).await;
    activate(&e.ctx.cra, &eustress_files());
    let st = state(&e, Some(SECRET), None, Arc::new(MockSlack::default()));
    assert_eq!(send(&st, &form("/msync-leaderboard", "", "U1000000102", "T1000000000")).await.status, StatusCode::OK);
    assert_eq!(send(&st, &form("/msync-leaderboard", "", "U1000000102", "T2000000000")).await.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn private_replies_are_ephemeral_and_public_ones_in_channel() {
    let e = env_with(true).await;
    let st = state(&e, Some(SECRET), Some("T1000000000"), Arc::new(MockSlack::default()));
    let (kind, text) = json_text(&send(&st, &form("/msync-payout-setup", "", "U1000000102", "T1000000000")).await);
    assert_eq!(kind, "ephemeral");
    assert!(text.contains("connect.stripe.test"), "{text}");
    let (kind, text) = json_text(&send(&st, &form("/msync-leaderboard", "", "U1000000102", "T1000000000")).await);
    assert_eq!(kind, "in_channel");
    assert!(text.contains("*Top Contributors*") && !text.contains("**"), "Slack bold is single-starred: {text}");
    let (kind, text) = json_text(&send(&st, &form("/msync-nope", "", "U1000000102", "T1000000000")).await);
    assert_eq!((kind.as_str(), text.as_str()), ("ephemeral", "Unknown command"));
}

#[tokio::test]
async fn the_slack_reply_is_the_shared_reply_translated() {
    let e = env_with(true).await;
    let st = state(&e, Some(SECRET), Some("T1000000000"), Arc::new(MockSlack::default()));
    let shared = crate::chat::commands::leaderboard(&e.ctx, &ChatActor::discord("100000000000000102", None)).await;
    let (_, text) = json_text(&send(&st, &form("/msync-leaderboard", "", "U1000000102", "T1000000000")).await);
    assert_eq!(text, to_mrkdwn(&shared.text));
}

#[tokio::test]
async fn slack_admin_commands_ask_the_web_api_and_fail_closed_when_it_cannot_answer() {
    let e = env_with(false).await;
    // users.info failed: unknown, so refused.
    let api = Arc::new(MockSlack { admin: None, ..Default::default() });
    let st = state(&e, Some(SECRET), Some("T0TEAM0001"), api.clone());
    let (kind, text) = json_text(&send(&st, &form("/msync-sync", "", "U0ADMIN001", "T0TEAM0001")).await);
    assert_eq!(kind, "ephemeral");
    assert!(text.contains("Could not confirm"), "{text}");
    let (_, text) = json_text(&send(&st, &form("/msync-payouts", "", "U0ADMIN001", "T0TEAM0001")).await);
    assert!(text.contains("Could not confirm"), "{text}");
    assert_eq!(*api.admin_calls.lock().unwrap(), vec!["U0ADMIN001".to_string(), "U0ADMIN001".to_string()]);
    // Non-admin commands do not call the API.
    send(&st, &form("/msync-stats", "", "U0ADMIN001", "T0TEAM0001")).await;
    assert_eq!(api.admin_calls.lock().unwrap().len(), 2);
    // A reported admin with no CRA: allowed; Slack has nothing to sync without one.
    let api = Arc::new(MockSlack { admin: Some(true), ..Default::default() });
    let st = state(&e, Some(SECRET), Some("T0TEAM0001"), api.clone());
    let (_, text) = json_text(&send(&st, &form("/msync-sync", "", "U0ADMIN001", "T0TEAM0001")).await);
    assert!(text.contains("Role sync complete") && text.contains("Nothing to sync"), "{text}");
    assert!(api.updates.lock().unwrap().is_empty());
}

#[tokio::test]
async fn slack_sync_sets_every_user_group_to_its_cra_membership() {
    let e = env_with(true).await;
    let api = Arc::new(MockSlack::default());
    let st = state(&e, Some(SECRET), Some("T1000000000"), api.clone());
    let (_, text) = json_text(&send(&st, &form("/msync-sync", "", "U1000000102", "T1000000000")).await);
    assert!(text.contains("Role sync complete"), "{text}");
    let updates = api.updates.lock().unwrap().clone();
    let snap = crate::cra::load(&eustress_files(), crate::chat::tests::today()).unwrap();
    let want = crate::cra::slack_projection(&snap, &Default::default(), crate::chat::tests::today()).users_by_usergroup();
    for (group, users) in &want {
        if users.is_empty() {
            assert!(text.contains(group.as_str()), "an empty group must be named: {text}");
            assert!(!updates.iter().any(|(g, _)| g == group));
        } else {
            assert!(updates.contains(&(group.clone(), users.clone())), "{group}: {updates:?}");
        }
    }
    assert_eq!(updates.len(), want.values().filter(|u| !u.is_empty()).count());
}

#[tokio::test]
async fn the_actix_route_reads_the_slack_headers() {
    let e = env_with(false).await;
    let st = actix_web::web::Data::new(state(&e, Some(SECRET), Some("T0TEAM0001"), Arc::new(MockSlack::default())));
    let app = atest::init_service(App::new().app_data(st.clone()).service(slack_commands)).await;
    let body = form("/msync-leaderboard", "", "U0ALICE0001", "T0TEAM0001");
    let now = chrono::Utc::now().timestamp().to_string();
    let req = atest::TestRequest::post()
        .uri("/slack/commands")
        .insert_header(("X-Slack-Request-Timestamp", now.clone()))
        .insert_header(("X-Slack-Signature", slack_v0_signature(SECRET, &now, &body)))
        .insert_header(("Content-Type", "application/x-www-form-urlencoded"))
        .set_payload(body.clone())
        .to_request();
    let resp = atest::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = atest::read_body_json(resp).await;
    assert_eq!(v["response_type"], "in_channel");
    let req = atest::TestRequest::post().uri("/slack/commands").set_payload(body).to_request();
    assert_eq!(atest::call_service(&app, req).await.status(), StatusCode::UNAUTHORIZED);
}

#[test]
fn slash_commands_map_onto_the_shared_commands() {
    for c in ChatCommand::ALL {
        assert_eq!(command_for(&format!("/msync-{}", c.name())), Some(c));
        assert_eq!(command_for(&format!("/{}", c.name())), None, "unprefixed Slack commands are not ours");
    }
}

// ---------------------------------------------------------- the real client

#[tokio::test]
async fn web_api_users_info_is_admin_and_failures_are_unknown() {
    use mockito::Matcher;
    let mut server = mockito::Server::new_async().await;
    let api = SlackWebApi::new(Some(server.url()), Some("xoxb-test".into()));
    let m = server
        .mock("GET", "/users.info")
        .match_query(Matcher::UrlEncoded("user".into(), "U0ADMIN001".into()))
        .match_header("authorization", "Bearer xoxb-test")
        .with_body(r#"{"ok":true,"user":{"id":"U0ADMIN001","is_admin":true}}"#)
        .create_async()
        .await;
    assert_eq!(api.is_admin("U0ADMIN001").await, Some(true));
    m.assert_async().await;
    server
        .mock("GET", "/users.info")
        .match_query(Matcher::UrlEncoded("user".into(), "U0PLAIN001".into()))
        .with_body(r#"{"ok":true,"user":{"id":"U0PLAIN001","is_admin":false}}"#)
        .create_async()
        .await;
    assert_eq!(api.is_admin("U0PLAIN001").await, Some(false));
    server
        .mock("GET", "/users.info")
        .match_query(Matcher::UrlEncoded("user".into(), "U0GONE0001".into()))
        .with_body(r#"{"ok":false,"error":"user_not_found"}"#)
        .create_async()
        .await;
    assert_eq!(api.is_admin("U0GONE0001").await, None);
    server
        .mock("GET", "/users.info")
        .match_query(Matcher::UrlEncoded("user".into(), "U0DOWN0001".into()))
        .with_status(500)
        .create_async()
        .await;
    assert_eq!(api.is_admin("U0DOWN0001").await, None);
    // No token: unknown, and no request is made.
    let no_token = SlackWebApi::new(Some(server.url()), None);
    assert_eq!(no_token.is_admin("U0ADMIN001").await, None);
}

#[tokio::test]
async fn web_api_usergroup_update_sends_the_full_list() {
    use mockito::Matcher;
    let mut server = mockito::Server::new_async().await;
    let api = SlackWebApi::new(Some(server.url()), Some("xoxb-test".into()));
    let m = server
        .mock("POST", "/usergroups.users.update")
        .match_header("authorization", "Bearer xoxb-test")
        .match_body(Matcher::Json(serde_json::json!({"usergroup": "S1000000060", "users": "U1,U2"})))
        .with_body(r#"{"ok":true}"#)
        .create_async()
        .await;
    assert_eq!(api.set_usergroup_users("S1000000060", &["U1".into(), "U2".into()]).await, Ok(()));
    m.assert_async().await;
    server
        .mock("POST", "/usergroups.users.update")
        .match_body(Matcher::PartialJson(serde_json::json!({"usergroup": "S1000000099"})))
        .with_body(r#"{"ok":false,"error":"paid_teams_only"}"#)
        .create_async()
        .await;
    assert_eq!(api.set_usergroup_users("S1000000099", &["U1".into()]).await, Err("paid_teams_only".into()));
}
