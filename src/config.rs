use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::env;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub discord: DiscordConfig,
    pub github: GitHubConfig,
    pub coderabbit: CodeRabbitConfig,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeRabbitConfig {
    pub api_key: String,
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
                api_key: env::var("CODERABBIT_API_KEY")
                    .context("CODERABBIT_API_KEY not found in environment")?,
            },
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
