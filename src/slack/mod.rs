//! Slack adapter for the shared chat commands (contract 12.2).
//!
//! Slash commands arrive at `POST /slack/commands`, form-encoded. Every request is verified
//! BEFORE its body is parsed: `X-Slack-Signature` must be `v0=` plus the hex HMAC-SHA256 of
//! `v0:<X-Slack-Request-Timestamp>:<raw body>` under `MSYNC_SLACK_SIGNING_SECRET`, compared in
//! constant time, with the timestamp within 300 seconds of now. An unset or empty secret
//! rejects everything. The request's `team_id` must be the configured team
//! (`MSYNC_SLACK_TEAM_ID`) or the CRA's `[slack]` team.
//!
//! Slack slash commands are workspace-global, so MSync's are prefixed: `/msync-link`,
//! `/msync-stats`, `/msync-leaderboard`, `/msync-sync`, `/msync-payouts`,
//! `/msync-payout-setup`. A private reply is `ephemeral`; any other is `in_channel`.

pub mod api;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use actix_web::{http::StatusCode, post, web, HttpRequest, HttpResponse};
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::chat::{ChatActor, ChatCommand, ChatContext, ChatReply, ProjectionApplier, ProjectionPlan, SyncReport};

pub use api::{SlackApi, SlackWebApi};

/// Slack's replay window (seconds).
pub const SLACK_TOLERANCE_SECS: i64 = 300;

/// Verify a Slack request signature over the raw body.
pub fn verify_slack_signature(
    secret: &str,
    timestamp: Option<&str>,
    signature: Option<&str>,
    body: &[u8],
    now_unix: i64,
) -> bool {
    if secret.is_empty() {
        return false;
    }
    let (Some(ts), Some(sig)) = (timestamp, signature) else { return false };
    let Ok(t) = ts.trim().parse::<i64>() else { return false };
    if (now_unix - t).abs() > SLACK_TOLERANCE_SECS {
        return false;
    }
    let Some(hex_sig) = sig.trim().strip_prefix("v0=") else { return false };
    let Ok(provided) = hex::decode(hex_sig) else { return false };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else { return false };
    mac.update(b"v0:");
    mac.update(ts.as_bytes());
    mac.update(b":");
    mac.update(body);
    let expected = mac.finalize().into_bytes();
    bool::from(expected.as_slice().ct_eq(provided.as_slice()))
}

/// Compute a Slack `v0=` signature (tests and local tooling).
pub fn slack_v0_signature(secret: &str, timestamp: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key");
    mac.update(b"v0:");
    mac.update(timestamp.as_bytes());
    mac.update(b":");
    mac.update(body);
    format!("v0={}", hex::encode(mac.finalize().into_bytes()))
}

/// The slash command fields MSync reads. Slack sends more; they are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct SlackCommandForm {
    pub command: String,
    #[serde(default)]
    pub text: String,
    pub user_id: String,
    pub team_id: String,
    #[serde(default)]
    pub response_url: Option<String>,
}

/// `/msync-<name>` to the shared command.
pub fn command_for(slash: &str) -> Option<ChatCommand> {
    ChatCommand::from_name(slash.trim().strip_prefix("/msync-")?)
}

/// Discord markdown bold (`**x**`) to Slack mrkdwn bold (`*x*`). Everything else is the same.
pub fn to_mrkdwn(text: &str) -> String {
    text.replace("**", "*")
}

/// The JSON body Slack expects for an immediate reply.
pub fn reply_json(reply: &ChatReply) -> serde_json::Value {
    serde_json::json!({
        "response_type": if reply.private { "ephemeral" } else { "in_channel" },
        "text": to_mrkdwn(&reply.text),
    })
}

/// Everything the route needs.
pub struct SlackState {
    pub chat: Arc<ChatContext>,
    /// `MSYNC_SLACK_SIGNING_SECRET`. `None` (or empty) rejects every request.
    pub signing_secret: Option<String>,
    /// `MSYNC_SLACK_TEAM_ID`, the team allowed besides the CRA's `[slack]` team.
    pub team_id: Option<String>,
    pub api: Arc<dyn SlackApi>,
}

impl SlackState {
    pub fn enabled(&self) -> bool {
        self.signing_secret.as_deref().map_or(false, |s| !s.is_empty())
    }

    /// Is `team` the configured team or the CRA-listed one?
    pub fn team_allowed(&self, team: &str) -> bool {
        if team.is_empty() {
            return false;
        }
        let configured = self.team_id.as_deref().map_or(false, |t| t == team);
        let cra = self
            .chat
            .cra
            .active()
            .and_then(|a| a.snapshot.org.slack.as_ref().map(|s| s.team_id == team))
            .unwrap_or(false);
        configured || cra
    }
}

/// The outcome of one request: status and, for 200, the JSON body.
#[derive(Debug, Clone, PartialEq)]
pub struct SlackOutcome {
    pub status: StatusCode,
    pub body: Option<serde_json::Value>,
}

impl SlackOutcome {
    fn reject(status: StatusCode) -> Self {
        SlackOutcome { status, body: None }
    }
}

/// Verify, parse, check the team, run the command. `now_unix` is passed in for tests.
pub async fn handle_command(
    state: &SlackState,
    timestamp: Option<&str>,
    signature: Option<&str>,
    body: &[u8],
    now_unix: i64,
) -> SlackOutcome {
    // 1. Signature first; nothing in the body is looked at before it passes.
    let secret = state.signing_secret.as_deref().unwrap_or("");
    if !verify_slack_signature(secret, timestamp, signature, body, now_unix) {
        return SlackOutcome::reject(StatusCode::UNAUTHORIZED);
    }
    // 2. Parse.
    let Ok(text) = std::str::from_utf8(body) else { return SlackOutcome::reject(StatusCode::BAD_REQUEST) };
    let form = match web::Query::<SlackCommandForm>::from_query(text) {
        Ok(f) => f.into_inner(),
        Err(_) => return SlackOutcome::reject(StatusCode::BAD_REQUEST),
    };
    // 3. Only our workspace.
    if !state.team_allowed(&form.team_id) {
        tracing::warn!("Slack command from team {} refused: not the configured or CRA-listed team", form.team_id);
        return SlackOutcome::reject(StatusCode::FORBIDDEN);
    }
    // 4. Run.
    let reply = match command_for(&form.command) {
        None => ChatReply::private("Unknown command"),
        Some(cmd) => {
            let is_admin = if cmd.is_admin_only() { state.api.is_admin(&form.user_id).await } else { None };
            let actor = ChatActor::slack(form.team_id.clone(), form.user_id.clone(), is_admin);
            let applier = SlackApplier { api: state.api.clone() };
            crate::chat::commands::run(&state.chat, &actor, cmd, &form.text, &applier).await
        }
    };
    SlackOutcome { status: StatusCode::OK, body: Some(reply_json(&reply)) }
}

#[post("/slack/commands")]
pub async fn slack_commands(req: HttpRequest, body: web::Bytes, state: web::Data<SlackState>) -> HttpResponse {
    let header = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let ts = header("X-Slack-Request-Timestamp");
    let sig = header("X-Slack-Signature");
    let now = chrono::Utc::now().timestamp();
    let out = handle_command(&state, ts.as_deref(), sig.as_deref(), &body, now).await;
    match out.body {
        Some(b) => HttpResponse::build(out.status).json(b),
        None => HttpResponse::build(out.status).finish(),
    }
}

/// Applies the CRA projection to Slack user groups.
pub struct SlackApplier {
    pub api: Arc<dyn SlackApi>,
}

#[async_trait]
impl ProjectionApplier for SlackApplier {
    /// Set the full member list of every managed user group with `usergroups.users.update`.
    /// Slack refuses an empty user group, so a group nobody should be in is left unchanged
    /// and named in the reply.
    async fn apply(&self, plan: &ProjectionPlan) -> SyncReport {
        let mut report = SyncReport::default();
        for (group, users) in plan.by_group() {
            if users.is_empty() {
                report.notes.push(format!("User group `{group}` left unchanged: Slack does not allow an empty user group."));
                continue;
            }
            match self.api.set_usergroup_users(&group, &users).await {
                Ok(()) => report.synced += 1,
                Err(e) => {
                    tracing::warn!("sync: Slack user group {} failed: {}", group, e);
                    report.failed += 1;
                    report.notes.push(format!("User group `{group}` failed: {e}"));
                }
            }
        }
        report
    }

    /// No CRA: Slack user groups come from the CRA only, so there is nothing to apply.
    async fn apply_legacy(&self) -> SyncReport {
        SyncReport {
            notes: vec!["Nothing to sync: Slack user groups come from the CRA (`slack_usergroup` in `roles.toml`).".into()],
            ..Default::default()
        }
    }
}
