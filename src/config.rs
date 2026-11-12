use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::env;

use crate::forge::RepoRegistry;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub discord: DiscordConfig,
    pub github: GitHubConfig,
    pub coderabbit: CodeRabbitConfig,
    #[serde(default)]
    pub gitlab: GitLabConfig,
    /// Repositories whose events earn (`DSYNC_FORGE_REPOS`). Unset: GitHub as before, GitLab refused.
    #[serde(default)]
    pub forge_repos: RepoRegistry,
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

fn non_empty_env(key: &str) -> Option<String> {
    env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

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
            forge_repos: RepoRegistry::from_env().map_err(anyhow::Error::msg)?,
            database_url: env::var("DATABASE_URL")
                .unwrap_or_else(|_| "sqlite://dsync.db".to_string()),
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
