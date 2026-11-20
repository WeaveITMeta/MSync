//! OAuth sign-in clients: GitHub, GitLab, Discord, Slack ("Sign in with Slack", OpenID
//! Connect) and Google. Authorization code flow; PKCE S256 where the provider supports it.
//!
//! Each provider is enabled only when `MSYNC_OAUTH_<PROVIDER>_CLIENT_ID` and
//! `MSYNC_OAUTH_<PROVIDER>_CLIENT_SECRET` are both set. Every base URL is injectable, so tests
//! point the real HTTP client at a mock server.

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::Value;

use super::rules::Provider;
use crate::config::env_var;

/// One provider's endpoints and credentials.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub provider: Provider,
    pub client_id: String,
    pub client_secret: String,
    /// Where the browser is sent to authorize.
    pub authorize_url: String,
    /// Where the code is exchanged (server to server).
    pub token_url: String,
    /// The provider's API base for the profile calls.
    pub api_base: String,
    pub scope: &'static str,
    /// The provider verifies a PKCE `code_challenge` (S256).
    pub pkce: bool,
}

/// The provider's own base URLs (`authorize`, `token`, `api`).
fn default_urls(p: Provider) -> Option<(&'static str, &'static str, &'static str, &'static str, bool)> {
    Some(match p {
        Provider::Github => (
            "https://github.com/login/oauth/authorize",
            "https://github.com/login/oauth/access_token",
            "https://api.github.com",
            "read:user user:email",
            true,
        ),
        Provider::Gitlab => ("https://gitlab.com/oauth/authorize", "https://gitlab.com/oauth/token", "https://gitlab.com/api/v4", "read_user", true),
        Provider::Discord => (
            "https://discord.com/oauth2/authorize",
            "https://discord.com/api/oauth2/token",
            "https://discord.com/api",
            "identify email",
            true,
        ),
        // Sign in with Slack (OpenID Connect). Slack does not document PKCE for this flow.
        Provider::Slack => (
            "https://slack.com/openid/connect/authorize",
            "https://slack.com/api/openid.connect.token",
            "https://slack.com/api",
            "openid profile email",
            false,
        ),
        Provider::Google => (
            "https://accounts.google.com/o/oauth2/v2/auth",
            "https://oauth2.googleapis.com/token",
            "https://openidconnect.googleapis.com/v1",
            "openid email profile",
            true,
        ),
        Provider::Email => return None,
    })
}

/// The env name stem: `MSYNC_OAUTH_GITHUB`.
pub fn env_stem(p: Provider) -> String {
    format!("MSYNC_OAUTH_{}", p.as_str().to_ascii_uppercase())
}

impl ProviderConfig {
    /// Build from a lookup. `None` unless both the client id and the secret are set and non-empty.
    /// `MSYNC_OAUTH_<P>_AUTHORIZE_URL`, `_TOKEN_URL` and `_API_BASE` override the provider's
    /// URLs (a self-hosted GitLab, or tests).
    pub fn from_lookup<F: Fn(&str) -> Option<String>>(p: Provider, get: F) -> Option<ProviderConfig> {
        let (auth, token, api, scope, pkce) = default_urls(p)?;
        let stem = env_stem(p);
        let val = |k: &str| get(&format!("{stem}_{k}")).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let client_id = val("CLIENT_ID")?;
        let client_secret = if super::mutant::on("provider_needs_only_id") { val("CLIENT_SECRET").unwrap_or_default() } else { val("CLIENT_SECRET")? };
        Some(ProviderConfig {
            provider: p,
            client_id,
            client_secret,
            authorize_url: val("AUTHORIZE_URL").unwrap_or_else(|| auth.to_string()),
            token_url: val("TOKEN_URL").unwrap_or_else(|| token.to_string()),
            api_base: val("API_BASE").unwrap_or_else(|| api.to_string()).trim_end_matches('/').to_string(),
            scope,
            pkce,
        })
    }

    /// Point every URL at one base (tests: a mock server).
    pub fn for_test(p: Provider, base: &str) -> ProviderConfig {
        let (auth, token, api, scope, pkce) = default_urls(p).expect("an OAuth provider");
        let path = |u: &str| {
            let after_scheme = u.split_once("://").map(|(_, r)| r).unwrap_or(u);
            after_scheme.find('/').map(|i| after_scheme[i..].to_string()).unwrap_or_default()
        };
        ProviderConfig {
            provider: p,
            client_id: format!("{}-client", p.as_str()),
            client_secret: format!("{}-secret", p.as_str()),
            authorize_url: format!("{base}{}", path(auth)),
            token_url: format!("{base}{}", path(token)),
            api_base: format!("{base}{}", path(api)),
            scope,
            pkce,
        }
    }

    /// The browser redirect to the provider.
    pub fn authorize_redirect(&self, redirect_uri: &str, state: &str, code_challenge: &str) -> String {
        let mut q = vec![
            ("response_type", "code"),
            ("client_id", self.client_id.as_str()),
            ("redirect_uri", redirect_uri),
            ("scope", self.scope),
            ("state", state),
        ];
        if self.pkce {
            q.push(("code_challenge", code_challenge));
            q.push(("code_challenge_method", "S256"));
        }
        if self.provider == Provider::Slack || self.provider == Provider::Google {
            // Ask for a fresh login choice rather than silently reusing a provider session.
            q.push(("prompt", "select_account"));
        }
        let sep = if self.authorize_url.contains('?') { '&' } else { '?' };
        format!("{}{sep}{}", self.authorize_url, encode_query(&q))
    }
}

/// Every enabled provider from the environment.
pub fn providers_from_env() -> HashMap<Provider, ProviderConfig> {
    Provider::OAUTH
        .into_iter()
        .filter_map(|p| ProviderConfig::from_lookup(p, env_var).map(|c| (p, c)))
        .collect()
}

/// Percent-encode a query string (RFC 3986 unreserved characters pass through).
pub fn encode_query(pairs: &[(&str, &str)]) -> String {
    pairs.iter().map(|(k, v)| format!("{}={}", pct(k), pct(v))).collect::<Vec<_>>().join("&")
}

pub fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Who the provider says signed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderProfile {
    pub provider: Provider,
    /// The stable user id (never a username). Slack: `<team>:<user>`.
    pub subject: String,
    /// For display only.
    pub handle: Option<String>,
    pub display_name: Option<String>,
    /// The address the provider returned, if any.
    pub email: Option<String>,
    /// The provider itself says the address is verified.
    pub email_verified: bool,
    /// GitHub and GitLab: the login, recorded as verified for the account.
    pub login: Option<String>,
    /// Slack: the team (workspace) id.
    pub team: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("provider {0} is not enabled")]
    Disabled(&'static str),
    #[error("provider HTTP error: {0}")]
    Http(String),
    #[error("provider refused: {0}")]
    Refused(String),
    #[error("provider response missing {0}")]
    Missing(&'static str),
}

/// The provider's side of a sign-in: exchange the code and read who signed in.
#[async_trait]
pub trait ProviderApi: Send + Sync {
    async fn complete(&self, cfg: &ProviderConfig, code: &str, verifier: &str, redirect_uri: &str) -> Result<ProviderProfile, ProviderError>;
}

/// The real implementation over HTTP.
pub struct HttpProviderApi {
    http: reqwest::Client,
}

impl Default for HttpProviderApi {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpProviderApi {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent("msync-sign-in")
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        HttpProviderApi { http }
    }

    async fn json(&self, req: reqwest::RequestBuilder) -> Result<Value, ProviderError> {
        let resp = req.send().await.map_err(|e| ProviderError::Http(e.to_string()))?;
        let status = resp.status();
        let body: Value = resp.json().await.map_err(|e| ProviderError::Http(e.to_string()))?;
        if !status.is_success() {
            return Err(ProviderError::Refused(format!("HTTP {status}")));
        }
        // GitHub and Slack report errors in a 200 body.
        if let Some(e) = body.get("error").and_then(Value::as_str) {
            return Err(ProviderError::Refused(e.to_string()));
        }
        if body.get("ok") == Some(&Value::Bool(false)) {
            return Err(ProviderError::Refused("ok=false".into()));
        }
        Ok(body)
    }

    async fn exchange(&self, cfg: &ProviderConfig, code: &str, verifier: &str, redirect_uri: &str) -> Result<String, ProviderError> {
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", cfg.client_id.as_str()),
            ("client_secret", cfg.client_secret.as_str()),
        ];
        if cfg.pkce {
            form.push(("code_verifier", verifier));
        }
        let body = self
            .json(self.http.post(&cfg.token_url).header("Accept", "application/json").form(&form))
            .await?;
        body.get("access_token")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or(ProviderError::Missing("access_token"))
    }

    async fn get(&self, url: String, token: &str) -> Result<Value, ProviderError> {
        self.json(self.http.get(url).bearer_auth(token).header("Accept", "application/json")).await
    }
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string).filter(|x| !x.is_empty())
}

/// A numeric or string id as text.
fn id_text(v: &Value, k: &str) -> Option<String> {
    match v.get(k)? {
        Value::Number(n) => Some(n.to_string()),
        Value::String(x) if !x.is_empty() => Some(x.clone()),
        _ => None,
    }
}

#[async_trait]
impl ProviderApi for HttpProviderApi {
    async fn complete(&self, cfg: &ProviderConfig, code: &str, verifier: &str, redirect_uri: &str) -> Result<ProviderProfile, ProviderError> {
        let token = self.exchange(cfg, code, verifier, redirect_uri).await?;
        let api = &cfg.api_base;
        Ok(match cfg.provider {
            Provider::Github => {
                let u = self.get(format!("{api}/user"), &token).await?;
                let login = s(&u, "login").ok_or(ProviderError::Missing("login"))?;
                // Only an address GitHub marks verified; the primary one first.
                let emails = self.get(format!("{api}/user/emails"), &token).await.unwrap_or(Value::Null);
                let mut verified: Vec<(bool, String)> = emails
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter(|e| e.get("verified") == Some(&Value::Bool(true)))
                            .filter_map(|e| Some((e.get("primary") == Some(&Value::Bool(true)), s(e, "email")?)))
                            .collect()
                    })
                    .unwrap_or_default();
                verified.sort_by_key(|a| std::cmp::Reverse(a.0));
                let email = verified.into_iter().next().map(|(_, e)| e);
                ProviderProfile {
                    provider: Provider::Github,
                    subject: id_text(&u, "id").ok_or(ProviderError::Missing("id"))?,
                    handle: Some(login.clone()),
                    display_name: s(&u, "name"),
                    email_verified: email.is_some(),
                    email,
                    login: Some(login),
                    team: None,
                }
            }
            Provider::Gitlab => {
                let u = self.get(format!("{api}/user"), &token).await?;
                let login = s(&u, "username").ok_or(ProviderError::Missing("username"))?;
                // GitLab returns `confirmed_at` for the signed-in user; no confirmation, no trust.
                let confirmed = u.get("confirmed_at").is_some_and(|c| !c.is_null());
                let email = s(&u, "email");
                ProviderProfile {
                    provider: Provider::Gitlab,
                    subject: id_text(&u, "id").ok_or(ProviderError::Missing("id"))?,
                    handle: Some(login.clone()),
                    display_name: s(&u, "name"),
                    email_verified: confirmed && email.is_some(),
                    email,
                    login: Some(login),
                    team: None,
                }
            }
            Provider::Discord => {
                let u = self.get(format!("{api}/users/@me"), &token).await?;
                ProviderProfile {
                    provider: Provider::Discord,
                    subject: id_text(&u, "id").ok_or(ProviderError::Missing("id"))?,
                    handle: s(&u, "username"),
                    display_name: s(&u, "global_name").or_else(|| s(&u, "username")),
                    email: s(&u, "email"),
                    email_verified: u.get("verified") == Some(&Value::Bool(true)) && s(&u, "email").is_some(),
                    login: None,
                    team: None,
                }
            }
            Provider::Slack => {
                let u = self.get(format!("{api}/openid.connect.userInfo"), &token).await?;
                let user = s(&u, "https://slack.com/user_id").ok_or(ProviderError::Missing("user_id"))?;
                let team = s(&u, "https://slack.com/team_id").ok_or(ProviderError::Missing("team_id"))?;
                ProviderProfile {
                    provider: Provider::Slack,
                    subject: format!("{team}:{user}"),
                    handle: s(&u, "name"),
                    display_name: s(&u, "name"),
                    email: s(&u, "email"),
                    email_verified: u.get("email_verified") == Some(&Value::Bool(true)) && s(&u, "email").is_some(),
                    login: None,
                    team: Some(team),
                }
            }
            Provider::Google => {
                let u = self.get(format!("{api}/userinfo"), &token).await?;
                ProviderProfile {
                    provider: Provider::Google,
                    subject: s(&u, "sub").ok_or(ProviderError::Missing("sub"))?,
                    handle: s(&u, "email"),
                    display_name: s(&u, "name"),
                    email: s(&u, "email"),
                    email_verified: u.get("email_verified") == Some(&Value::Bool(true)) && s(&u, "email").is_some(),
                    login: None,
                    team: None,
                }
            }
            Provider::Email => return Err(ProviderError::Disabled("email")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_provider_is_enabled_only_with_both_values() {
        let only_id = |k: &str| (k == "MSYNC_OAUTH_GITHUB_CLIENT_ID").then(|| "id".to_string());
        assert!(ProviderConfig::from_lookup(Provider::Github, only_id).is_none());
        let both = |k: &str| match k {
            "MSYNC_OAUTH_GITHUB_CLIENT_ID" => Some("id".to_string()),
            "MSYNC_OAUTH_GITHUB_CLIENT_SECRET" => Some("secret".to_string()),
            _ => None,
        };
        let c = ProviderConfig::from_lookup(Provider::Github, both).unwrap();
        assert_eq!(c.token_url, "https://github.com/login/oauth/access_token");
        let blank = |k: &str| k.starts_with("MSYNC_OAUTH_GITHUB").then(|| "  ".to_string());
        assert!(ProviderConfig::from_lookup(Provider::Github, blank).is_none());
        assert!(ProviderConfig::from_lookup(Provider::Email, both).is_none());
    }

    #[test]
    fn the_authorize_redirect_carries_state_and_pkce() {
        let c = ProviderConfig::for_test(Provider::Github, "http://mock");
        let url = c.authorize_redirect("https://msync.example/auth/github/callback", "st&ate", "chal");
        assert!(url.starts_with("http://mock/login/oauth/authorize?response_type=code&client_id=github-client"));
        assert!(url.contains("redirect_uri=https%3A%2F%2Fmsync.example%2Fauth%2Fgithub%2Fcallback"));
        assert!(url.contains("state=st%26ate"));
        assert!(url.contains("code_challenge=chal&code_challenge_method=S256"));
        let slack = ProviderConfig::for_test(Provider::Slack, "http://mock");
        assert!(!slack.authorize_redirect("r", "s", "c").contains("code_challenge"));
        assert_eq!(slack.token_url, "http://mock/api/openid.connect.token");
    }
}
