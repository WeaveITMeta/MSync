//! The Slack Web API calls MSync makes, behind a trait so tests use a mock. The base URL is
//! injectable (`MSYNC_SLACK_API_BASE`, default `https://slack.com/api`).

use async_trait::async_trait;
use serde_json::Value;

use crate::chat::sync::PlatformError;

pub const DEFAULT_SLACK_API_BASE: &str = "https://slack.com/api";
/// Slack's `response_url`s live here. MSync posts a deferred reply only to a URL with this
/// prefix, so a request can never make MSync post somewhere else.
pub const DEFAULT_RESPONSE_URL_PREFIX: &str = "https://hooks.slack.com/";

#[async_trait]
pub trait SlackApi: Send + Sync {
    /// A bot token is configured. Role sync skips Slack without one.
    fn configured(&self) -> bool {
        true
    }

    /// `users.info` `is_admin` for a user. `None` when it cannot be read (no token, an API
    /// error, a network error): callers treat that as "unknown", which fails closed.
    async fn is_admin(&self, user_id: &str) -> Option<bool>;

    /// `usergroups.users.list`: the full member list of a user group.
    async fn usergroup_users(&self, usergroup_id: &str) -> Result<Vec<String>, PlatformError>;

    /// `usergroups.users.update`: replace the full member list of a user group.
    async fn set_usergroup_users(
        &self,
        usergroup_id: &str,
        user_ids: &[String],
    ) -> Result<(), PlatformError>;

    /// `chat.postMessage` to a channel.
    async fn post_message(&self, channel: &str, text: &str) -> Result<(), PlatformError>;

    /// POST a deferred slash-command reply to its `response_url`.
    async fn respond(&self, response_url: &str, body: &Value) -> Result<(), String>;
}

/// The real Web API client.
pub struct SlackWebApi {
    base: String,
    token: Option<String>,
    response_url_prefix: String,
    http: reqwest::Client,
}

impl SlackWebApi {
    pub fn new(base: Option<String>, token: Option<String>) -> Self {
        SlackWebApi {
            base: base
                .unwrap_or_else(|| DEFAULT_SLACK_API_BASE.to_string())
                .trim_end_matches('/')
                .to_string(),
            token: token.filter(|t| !t.trim().is_empty()),
            response_url_prefix: DEFAULT_RESPONSE_URL_PREFIX.to_string(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    /// Allow `response_url`s under another prefix (tests against a mock server).
    pub fn with_response_url_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.response_url_prefix = prefix.into();
        self
    }

    fn token(&self) -> Result<&str, PlatformError> {
        self.token
            .as_deref()
            .ok_or_else(|| PlatformError::Api("MSYNC_SLACK_BOT_TOKEN is not set".into()))
    }
}

fn ok_body(v: &Value) -> Result<(), PlatformError> {
    if v.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        let e = v
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        if e == "ratelimited" {
            Err(PlatformError::RateLimited {
                retry_after_secs: None,
            })
        } else {
            Err(PlatformError::Api(e.to_string()))
        }
    }
}

/// HTTP status to [`PlatformError`]; 429 carries `Retry-After`.
async fn json_of(resp: reqwest::Response) -> Result<Value, PlatformError> {
    if resp.status().as_u16() == 429 {
        let retry = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        return Err(PlatformError::RateLimited {
            retry_after_secs: retry,
        });
    }
    if !resp.status().is_success() {
        return Err(PlatformError::Api(format!("HTTP {}", resp.status())));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| PlatformError::Api(e.to_string()))?;
    ok_body(&v)?;
    Ok(v)
}

#[async_trait]
impl SlackApi for SlackWebApi {
    fn configured(&self) -> bool {
        self.token.is_some()
    }

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

    async fn usergroup_users(&self, usergroup_id: &str) -> Result<Vec<String>, PlatformError> {
        let token = self.token()?;
        let resp = self
            .http
            .get(format!("{}/usergroups.users.list", self.base))
            .bearer_auth(token)
            .query(&[("usergroup", usergroup_id)])
            .send()
            .await
            .map_err(|e| PlatformError::Api(e.to_string()))?;
        let v = json_of(resp).await?;
        Ok(v.get("users")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|u| u.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn set_usergroup_users(
        &self,
        usergroup_id: &str,
        user_ids: &[String],
    ) -> Result<(), PlatformError> {
        let token = self.token()?;
        let resp = self
            .http
            .post(format!("{}/usergroups.users.update", self.base))
            .bearer_auth(token)
            .json(&serde_json::json!({ "usergroup": usergroup_id, "users": user_ids.join(",") }))
            .send()
            .await
            .map_err(|e| PlatformError::Api(e.to_string()))?;
        json_of(resp).await.map(|_| ())
    }

    async fn post_message(&self, channel: &str, text: &str) -> Result<(), PlatformError> {
        let token = self.token()?;
        let resp = self
            .http
            .post(format!("{}/chat.postMessage", self.base))
            .bearer_auth(token)
            .json(&serde_json::json!({ "channel": channel, "text": text }))
            .send()
            .await
            .map_err(|e| PlatformError::Api(e.to_string()))?;
        json_of(resp).await.map(|_| ())
    }

    async fn respond(&self, response_url: &str, body: &Value) -> Result<(), String> {
        if !response_url.starts_with(&self.response_url_prefix) {
            return Err(format!(
                "response_url outside {} refused",
                self.response_url_prefix
            ));
        }
        let resp = self
            .http
            .post(response_url)
            .json(body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!("HTTP {}", resp.status()))
        }
    }
}
