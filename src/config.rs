use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::env;

use crate::cra::source::CraRepo;
use crate::foundry::RepoRegistry;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub discord: DiscordConfig,
    pub github: GitHubConfig,
    pub coderabbit: CodeRabbitConfig,
    #[serde(default)]
    pub gitlab: GitLabConfig,
    /// Repositories whose events earn (`MSYNC_FOUNDRY_REPOS`). Unset: GitHub as before, GitLab refused.
    #[serde(default)]
    pub foundry_repos: RepoRegistry,
    /// The CRA repository (`MSYNC_CRA_REPO`). Unset: no CRA loading; everything as before.
    #[serde(default)]
    pub cra: CraConfig,
    pub database_url: String,
    pub server: ServerConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordConfig {
    pub token: String,
    pub application_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubConfig {
    pub token: String,
    pub webhook_secret: String,
    pub repo_owner: String,
    pub repo_name: String,
}

/// CodeRabbit is optional (an unset or empty `CODERABBIT_API_KEY` is None).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodeRabbitConfig {
    pub api_key: Option<String>,
}

/// GitLab webhooks. With no secret (unset or empty `GITLAB_WEBHOOK_SECRET`) every
/// GitLab webhook is rejected.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitLabConfig {
    pub webhook_secret: Option<String>,
}

/// CRA loading (contract section 4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CraConfig {
    /// `provider:host:path` of the CRA repository (`MSYNC_CRA_REPO`).
    pub repo: Option<CraRepo>,
    /// The branch whose pushes load the CRA (`MSYNC_CRA_BRANCH`, default `main`).
    pub branch: String,
    /// Read token for a GitLab CRA repository (`GITLAB_TOKEN`). GitHub uses `GITHUB_TOKEN`.
    #[serde(skip_serializing)]
    pub gitlab_token: Option<String>,
    /// API base overrides (`MSYNC_CRA_GITHUB_API`, `MSYNC_CRA_GITLAB_API`), for GitHub
    /// Enterprise or tests. Default: `https://api.github.com`, `https://<host>/api/v4`.
    pub github_api_base: Option<String>,
    pub gitlab_api_base: Option<String>,
}

impl Default for CraConfig {
    fn default() -> Self {
        CraConfig { repo: None, branch: "main".into(), gitlab_token: None, github_api_base: None, gitlab_api_base: None }
    }
}

impl CraConfig {
    pub fn from_env() -> Result<Self> {
        let repo = match non_empty_env("MSYNC_CRA_REPO") {
            Some(spec) => Some(CraRepo::parse(&spec).map_err(anyhow::Error::msg)?),
            None => None,
        };
        let branch = non_empty_env("MSYNC_CRA_BRANCH").unwrap_or_else(|| "main".into());
        if branch.contains(char::is_whitespace) || branch.starts_with("refs/") {
            anyhow::bail!("MSYNC_CRA_BRANCH must be a bare branch name, e.g. main");
        }
        Ok(CraConfig {
            repo,
            branch,
            gitlab_token: non_empty_env("GITLAB_TOKEN"),
            github_api_base: non_empty_env("MSYNC_CRA_GITHUB_API"),
            gitlab_api_base: non_empty_env("MSYNC_CRA_GITLAB_API"),
        })
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    env_var(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// Prefix of every MSync environment variable.
pub const ENV_PREFIX: &str = "MSYNC_";
/// The pre-rename prefix (contract section 12.1). Still read when the `MSYNC_` twin is
/// unset, because a running deployment may have the old names set.
const LEGACY_ENV_PREFIX: &str = "DSYNC_";

/// Read an environment variable through the MSync naming rule.
///
/// `name` is the full new name (`MSYNC_PAY_MAX_CENTS`). It wins when set. Only when it is
/// unset is the old twin (`DSYNC_PAY_MAX_CENTS`) read, with a one-time warning naming it.
/// A name without the `MSYNC_` prefix (`DATABASE_URL`, `STRIPE_SECRET_KEY`) is read as is.
pub fn env_var(name: &str) -> Option<String> {
    env_var_with(name, |k| env::var(k).ok())
}

/// [`env_var`] over an injected lookup (tests, `PaymentConfig::from_lookup`).
pub fn env_var_with<F: Fn(&str) -> Option<String>>(name: &str, get: F) -> Option<String> {
    if let Some(v) = get(name) {
        return Some(v);
    }
    let legacy = legacy_env_name(name)?;
    let v = get(&legacy)?;
    warn_legacy_once(&legacy, name);
    Some(v)
}

/// `MSYNC_X` -> `DSYNC_X`; None for a name outside the MSync prefix.
pub fn legacy_env_name(name: &str) -> Option<String> {
    name.strip_prefix(ENV_PREFIX).map(|rest| format!("{LEGACY_ENV_PREFIX}{rest}"))
}

fn warn_legacy_once(legacy: &str, name: &str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let first = WARNED
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .map(|mut seen| seen.insert(legacy.to_string()))
        .unwrap_or(false);
    if first {
        tracing::warn!("{legacy} is deprecated; rename it to {name}. It is read only because {name} is unset.");
    }
}

/// Default database when `DATABASE_URL` is unset. Keeps its pre-rename file name on purpose
/// (contract 12.1): changing it would start an existing deployment on an empty database.
pub const DEFAULT_DATABASE_URL: &str = "sqlite://dsync.db";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Config {
            discord: DiscordConfig {
                token: env::var("DISCORD_TOKEN")
                    .context("DISCORD_TOKEN not found in environment")?,
                application_id: env::var("DISCORD_APPLICATION_ID")
                    .context("DISCORD_APPLICATION_ID not found in environment")?
                    .parse()
                    .context("DISCORD_APPLICATION_ID must be a valid u64")?,
            },
            github: GitHubConfig {
                token: env::var("GITHUB_TOKEN")
                    .context("GITHUB_TOKEN not found in environment")?,
                webhook_secret: env::var("GITHUB_WEBHOOK_SECRET")
                    .context("GITHUB_WEBHOOK_SECRET not found in environment")?,
                repo_owner: env::var("GITHUB_REPO_OWNER")
                    .context("GITHUB_REPO_OWNER not found in environment")?,
                repo_name: env::var("GITHUB_REPO_NAME")
                    .context("GITHUB_REPO_NAME not found in environment")?,
            },
            coderabbit: CodeRabbitConfig {
                api_key: non_empty_env("CODERABBIT_API_KEY"),
            },
            gitlab: GitLabConfig {
                webhook_secret: non_empty_env("GITLAB_WEBHOOK_SECRET"),
            },
            foundry_repos: RepoRegistry::from_env().map_err(anyhow::Error::msg)?,
            cra: CraConfig::from_env()?,
            database_url: env::var("DATABASE_URL")
                .unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string()),
            server: ServerConfig {
                host: env::var("SERVER_HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
                port: env::var("SERVER_PORT")
                    .unwrap_or_else(|_| "8080".to_string())
                    .parse()
                    .context("SERVER_PORT must be a valid port number")?,
            },
        })
    }
}

/// Env var name holding the Discord role ID for a merit role,
/// e.g. "Active Contributor" -> "DISCORD_ROLE_ID_ACTIVE_CONTRIBUTOR".
pub fn discord_role_env_var(role_name: &str) -> String {
    format!("DISCORD_ROLE_ID_{}", role_name.trim().to_uppercase().replace(' ', "_"))
}

/// Discord role ID for a merit role from the environment, or None if unset/invalid.
pub fn discord_role_id_for(role_name: &str) -> Option<i64> {
    env::var(discord_role_env_var(role_name))
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|id| *id != 0)
        .map(|id| id as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k: &str| m.get(k).cloned()
    }

    #[test]
    fn msync_name_wins_over_its_dsync_twin() {
        let get = lookup(&[("MSYNC_X", "new"), ("DSYNC_X", "old")]);
        assert_eq!(env_var_with("MSYNC_X", &get).as_deref(), Some("new"));
    }

    #[test]
    fn dsync_name_alone_is_still_read() {
        let get = lookup(&[("DSYNC_X", "old")]);
        assert_eq!(env_var_with("MSYNC_X", &get).as_deref(), Some("old"));
    }

    #[test]
    fn neither_name_set_gives_none_so_the_caller_default_applies() {
        let get = lookup(&[("DSYNC_Y", "other"), ("MSYNC_Y", "other")]);
        assert_eq!(env_var_with("MSYNC_X", &get), None);
        assert_eq!(env_var_with("MSYNC_X", &get).unwrap_or_else(|| "default".into()), "default");
    }

    #[test]
    fn names_outside_the_prefix_have_no_fallback() {
        let get = lookup(&[("DSYNC_DATABASE_URL", "x"), ("DATABASE_URL", "sqlite://a.db")]);
        assert_eq!(env_var_with("DATABASE_URL", &get).as_deref(), Some("sqlite://a.db"));
        assert_eq!(env_var_with("STRIPE_SECRET_KEY", &get), None);
        assert_eq!(legacy_env_name("MSYNC_PAY_MAX_CENTS").as_deref(), Some("DSYNC_PAY_MAX_CENTS"));
        assert_eq!(legacy_env_name("DATABASE_URL"), None);
    }

    /// Pinned (contract 12.1): the default database keeps its pre-rename file name.
    #[test]
    fn default_database_url_keeps_its_old_name() {
        assert_eq!(DEFAULT_DATABASE_URL, "sqlite://dsync.db");
    }
}
