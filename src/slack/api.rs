//! The Slack Web API calls MSync makes, behind a trait so tests use a mock. The base URL is
//! injectable (`MSYNC_SLACK_API_BASE`, default `https://slack.com/api`).

use async_trait::async_trait;
use serde_json::Value;

pub const DEFAULT_SLACK_API_BASE: &str = "https://slack.com/api";

#[async_trait]
pub trait SlackApi: Send + Sync {
    /// `users.info` `is_admin` for a user. `None` when it cannot be read (no token, an API
    /// error, a network error): callers treat that as "unknown", which fails closed.
    async fn is_admin(&self, user_id: &str) -> Option<bool>;

    /// `usergroups.users.update`: replace the full member list of a user group.
    async fn set_usergroup_users(&self, usergroup_id: &str, user_ids: &[String]) -> Result<(), String>;
}

/// The real Web API client.
pub struct SlackWebApi {
    base: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl SlackWebApi {
    pub fn new(base: Option<String>, token: Option<String>) -> Self {
        SlackWebApi {
            base: base.unwrap_or_else(|| DEFAULT_SLACK_API_BASE.to_string()).trim_end_matches('/').to_string(),
            token: token.filter(|t| !t.trim().is_empty()),
            http: reqwest::Client::new(),
        }
    }
}

fn ok_body(v: &Value) -> Result<(), String> {
    if v.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(v.get("error").and_then(Value::as_str).unwrap_or("unknown error").to_string())
    }
}

#[async_trait]
impl SlackApi for SlackWebApi {
    async fn is_admin(&self, user_id: &str) -> Option<bool> {
        let token = self.token.as_deref()?;
        let resp = self
            .http
            .get(format!("{}/users.info", self.base))
            .bearer_auth(token)
            .query(&[("user", user_id)])
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: Value = resp.json().await.ok()?;
        ok_body(&v).ok()?;
        v.get("user")?.get("is_admin")?.as_bool()
    }

    async fn set_usergroup_users(&self, usergroup_id: &str, user_ids: &[String]) -> Result<(), String> {
        let token = self.token.as_deref().ok_or("MSYNC_SLACK_BOT_TOKEN is not set")?;
        let resp = self
            .http
            .post(format!("{}/usergroups.users.update", self.base))
            .bearer_auth(token)
            .json(&serde_json::json!({ "usergroup": usergroup_id, "users": user_ids.join(",") }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        let v: Value = resp.json().await.map_err(|e| e.to_string())?;
        ok_body(&v)
    }
}
