use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::env;

use crate::cra::source::CraRepo;
use crate::foundry::RepoRegistry;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// The Discord bot (`DISCORD_TOKEN`, `DISCORD_APPLICATION_ID`). `None` runs without
    /// Discord. At least one of Discord and Slack must be configured.
    #[serde(default)]
    pub discord: Option<DiscordConfig>,
    /// The Slack app (`MSYNC_SLACK_*`). Enabled only when the signing secret is set.
    #[serde(default)]
    pub slack: SlackConfig,
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
    /// `MSYNC_DEV_MODE=true`: local development. Missing chat platforms are a warning, a
    /// local CRA directory may stand in for the CRA repository, `/health` reports
    /// `"dev_mode": true`, and a live Stripe configuration refuses to start. Never on a
    /// reachable host.
    #[serde(default)]
    pub dev_mode: bool,
}

/// Is `MSYNC_DEV_MODE` set to `true` (or `1`)?
pub fn dev_mode_from<F: Fn(&str) -> Option<String>>(get: F) -> bool {
    env_var_with("MSYNC_DEV_MODE", &get)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1"))
        .unwrap_or(false)
}

/// Refuse a dev-mode start with a live Stripe configuration: `MSYNC_STRIPE_ALLOW_LIVE=true`,
/// or a `sk_live_` / `rk_live_` key in `STRIPE_SECRET_KEY`. Dev mode is unsafe on purpose, so
/// it must never be able to move real money. Outside dev mode this check does nothing.
pub fn check_dev_mode_stripe<F: Fn(&str) -> Option<String>>(get: F) -> Result<()> {
    if !dev_mode_from(&get) {
        return Ok(());
    }
    let allow_live = env_var_with("MSYNC_STRIPE_ALLOW_LIVE", &get)
        .map(|v| v.trim() == "true")
        .unwrap_or(false);
    if allow_live {
        anyhow::bail!("MSYNC_DEV_MODE=true refuses to start with MSYNC_STRIPE_ALLOW_LIVE=true; dev mode never moves real money");
    }
    let key = get("STRIPE_SECRET_KEY").unwrap_or_default();
    let key = key.trim();
    if key.contains("_live_") {
        anyhow::bail!("MSYNC_DEV_MODE=true refuses to start with a live Stripe key in STRIPE_SECRET_KEY; use a test key (sk_test_) or none");
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordConfig {
    pub token: String,
    pub application_id: u64,
}

/// The Slack app (contract 12.2). Every field is optional; Slack is enabled only when
/// `signing_secret` is set, and with no secret every Slack request is rejected.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackConfig {
    /// `MSYNC_SLACK_SIGNING_SECRET`.
    #[serde(skip_serializing, default)]
    pub signing_secret: Option<String>,
    /// `MSYNC_SLACK_BOT_TOKEN` (`xoxb-...`), for `users.info` and `usergroups.users.update`.
    #[serde(skip_serializing, default)]
    pub bot_token: Option<String>,
    /// `MSYNC_SLACK_TEAM_ID`: the workspace allowed to send commands, besides the CRA's
    /// `[slack]` team.
    pub team_id: Option<String>,
    /// `MSYNC_SLACK_API_BASE`, for tests. Default `https://slack.com/api`.
    pub api_base: Option<String>,
}

impl SlackConfig {
    pub fn enabled(&self) -> bool {
        self.signing_secret.is_some()
    }
}

/// Discord and Slack settings from a lookup (the environment, or a map in tests).
///
/// Discord is configured when `DISCORD_TOKEN` is set and not empty; it then needs
/// `DISCORD_APPLICATION_ID`. Slack is configured when `MSYNC_SLACK_SIGNING_SECRET` is set
/// and not empty. Neither configured is an error (the service would have no chat platform),
/// except in dev mode (`MSYNC_DEV_MODE=true`), where it is a warning.
pub fn chat_platforms_from<F: Fn(&str) -> Option<String>>(
    get: F,
) -> Result<(Option<DiscordConfig>, SlackConfig)> {
    let non_empty = |k: &str| {
        env_var_with(k, &get)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let discord = match non_empty("DISCORD_TOKEN") {
        Some(token) => Some(DiscordConfig {
            token,
            application_id: non_empty("DISCORD_APPLICATION_ID")
                .context(
                    "DISCORD_APPLICATION_ID not found in environment (needed with DISCORD_TOKEN)",
                )?
                .parse()
                .context("DISCORD_APPLICATION_ID must be a valid u64")?,
        }),
        None => None,
    };
    let slack = SlackConfig {
        signing_secret: non_empty("MSYNC_SLACK_SIGNING_SECRET"),
        bot_token: non_empty("MSYNC_SLACK_BOT_TOKEN"),
        team_id: non_empty("MSYNC_SLACK_TEAM_ID"),
        api_base: non_empty("MSYNC_SLACK_API_BASE"),
    };
    if discord.is_none() && !slack.enabled() {
        if dev_mode_from(&get) {
            tracing::warn!("no chat platform configured (dev mode): Discord and Slack are off; the REST API and MCP server still run");
            return Ok((discord, slack));
        }
        anyhow::bail!(
            "no chat platform configured: set DISCORD_TOKEN and DISCORD_APPLICATION_ID for Discord, \
             MSYNC_SLACK_SIGNING_SECRET for Slack, or both"
        );
    }
    Ok((discord, slack))
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
    /// `MSYNC_CRA_LOCAL_DIR`: a directory holding `.msync/` that stands in for the CRA
    /// repository in development. Refused unless `MSYNC_DEV_MODE=true`.
    #[serde(default)]
    pub local_dir: Option<std::path::PathBuf>,
}

impl Default for CraConfig {
    fn default() -> Self {
        CraConfig {
            repo: None,
            branch: "main".into(),
            gitlab_token: None,
            github_api_base: None,
            gitlab_api_base: None,
            local_dir: None,
        }
    }
}

/// `MSYNC_CRA_LOCAL_DIR`, checked: refused unless dev mode is on, refused together with
/// `MSYNC_CRA_REPO`, and the directory must contain `.msync/`.
pub fn local_cra_dir_from<F: Fn(&str) -> Option<String>>(
    get: F,
) -> Result<Option<std::path::PathBuf>> {
    let Some(dir) = env_var_with("MSYNC_CRA_LOCAL_DIR", &get)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    else {
        return Ok(None);
    };
    if !dev_mode_from(&get) {
        anyhow::bail!(
            "MSYNC_CRA_LOCAL_DIR is set but MSYNC_DEV_MODE is not true: a local directory may stand in for the CRA \
             repository only in development. Unset MSYNC_CRA_LOCAL_DIR, or set MSYNC_DEV_MODE=true on a machine \
             nobody else can reach"
        );
    }
    if env_var_with("MSYNC_CRA_REPO", &get).is_some_and(|v| !v.trim().is_empty()) {
        anyhow::bail!("set MSYNC_CRA_REPO or MSYNC_CRA_LOCAL_DIR, not both");
    }
    let path = std::path::PathBuf::from(&dir);
    if !path.join(".msync").is_dir() {
        anyhow::bail!("MSYNC_CRA_LOCAL_DIR={dir}: no .msync/ directory there");
    }
    Ok(Some(path))
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
            local_dir: local_cra_dir_from(|k| env::var(k).ok())?,
        })
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    env_var(key)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
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
    name.strip_prefix(ENV_PREFIX)
        .map(|rest| format!("{LEGACY_ENV_PREFIX}{rest}"))
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
        tracing::warn!(
            "{legacy} is deprecated; rename it to {name}. It is read only because {name} is unset."
        );
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
        let get = |k: &str| env::var(k).ok();
        check_dev_mode_stripe(get)?;
        let (discord, slack) = chat_platforms_from(get)?;
        Ok(Config {
            discord,
            slack,
            // GitHub is optional: with no webhook secret every GitHub webhook is refused
            // (`verify_github_signature` rejects an empty secret), and with no token the
            // GitHub API is called unauthenticated.
            github: GitHubConfig {
                token: env::var("GITHUB_TOKEN").unwrap_or_default(),
                webhook_secret: env::var("GITHUB_WEBHOOK_SECRET").unwrap_or_default(),
                repo_owner: env::var("GITHUB_REPO_OWNER").unwrap_or_default(),
                repo_name: env::var("GITHUB_REPO_NAME").unwrap_or_default(),
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
            dev_mode: dev_mode_from(get),
        })
    }
}

/// Env var name holding the Discord role ID for a merit role,
/// e.g. "Active Contributor" -> "DISCORD_ROLE_ID_ACTIVE_CONTRIBUTOR".
pub fn discord_role_env_var(role_name: &str) -> String {
    format!(
        "DISCORD_ROLE_ID_{}",
        role_name.trim().to_uppercase().replace(' ', "_")
    )
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
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
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
        assert_eq!(
            env_var_with("MSYNC_X", &get).unwrap_or_else(|| "default".into()),
            "default"
        );
    }

    #[test]
    fn names_outside_the_prefix_have_no_fallback() {
        let get = lookup(&[
            ("DSYNC_DATABASE_URL", "x"),
            ("DATABASE_URL", "sqlite://a.db"),
        ]);
        assert_eq!(
            env_var_with("DATABASE_URL", &get).as_deref(),
            Some("sqlite://a.db")
        );
        assert_eq!(env_var_with("STRIPE_SECRET_KEY", &get), None);
        assert_eq!(
            legacy_env_name("MSYNC_PAY_MAX_CENTS").as_deref(),
            Some("DSYNC_PAY_MAX_CENTS")
        );
        assert_eq!(legacy_env_name("DATABASE_URL"), None);
    }

    #[test]
    fn discord_only_slack_only_and_both_configure() {
        let (d, s) = chat_platforms_from(lookup(&[
            ("DISCORD_TOKEN", "t"),
            ("DISCORD_APPLICATION_ID", "7"),
        ]))
        .unwrap();
        assert_eq!(d.map(|d| d.application_id), Some(7));
        assert!(!s.enabled());
        let (d, s) = chat_platforms_from(lookup(&[
            ("MSYNC_SLACK_SIGNING_SECRET", "sec"),
            ("MSYNC_SLACK_TEAM_ID", "T1"),
        ]))
        .unwrap();
        assert!(d.is_none(), "Slack only: no Discord token is needed");
        assert!(s.enabled());
        assert_eq!(s.team_id.as_deref(), Some("T1"));
        let (d, s) = chat_platforms_from(lookup(&[
            ("DISCORD_TOKEN", "t"),
            ("DISCORD_APPLICATION_ID", "7"),
            ("MSYNC_SLACK_SIGNING_SECRET", "sec"),
        ]))
        .unwrap();
        assert!(d.is_some() && s.enabled());
    }

    #[test]
    fn no_chat_platform_fails_startup() {
        assert!(chat_platforms_from(lookup(&[])).is_err());
        // An empty secret or token does not count.
        assert!(chat_platforms_from(lookup(&[
            ("MSYNC_SLACK_SIGNING_SECRET", " "),
            ("DISCORD_TOKEN", "")
        ]))
        .is_err());
        // A Discord token without its application id is an error, not "Discord off".
        assert!(chat_platforms_from(lookup(&[
            ("DISCORD_TOKEN", "t"),
            ("MSYNC_SLACK_SIGNING_SECRET", "s")
        ]))
        .is_err());
    }

    #[test]
    fn dev_mode_makes_missing_chat_platforms_a_warning() {
        let (d, s) = chat_platforms_from(lookup(&[("MSYNC_DEV_MODE", "true")])).unwrap();
        assert!(d.is_none() && !s.enabled());
        // Anything but true/1 is not dev mode: the existing rule stands.
        assert!(chat_platforms_from(lookup(&[("MSYNC_DEV_MODE", "yes")])).is_err());
        assert!(chat_platforms_from(lookup(&[("MSYNC_DEV_MODE", "false")])).is_err());
        assert!(dev_mode_from(lookup(&[("MSYNC_DEV_MODE", "1")])));
        assert!(!dev_mode_from(lookup(&[])));
    }

    #[test]
    fn local_cra_dir_is_refused_without_dev_mode() {
        let dir = std::env::temp_dir().join(format!("msync-local-cra-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join(".msync")).unwrap();
        let d = dir.to_string_lossy().to_string();
        let err = local_cra_dir_from(lookup(&[("MSYNC_CRA_LOCAL_DIR", &d)]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("MSYNC_DEV_MODE"), "{err}");
        let err = local_cra_dir_from(lookup(&[
            ("MSYNC_CRA_LOCAL_DIR", &d),
            ("MSYNC_DEV_MODE", "false"),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("MSYNC_DEV_MODE"));
        assert_eq!(
            local_cra_dir_from(lookup(&[
                ("MSYNC_CRA_LOCAL_DIR", &d),
                ("MSYNC_DEV_MODE", "true")
            ]))
            .unwrap(),
            Some(std::path::PathBuf::from(&d))
        );
        // Not together with a real CRA repository, and the directory must hold .msync/.
        assert!(local_cra_dir_from(lookup(&[
            ("MSYNC_CRA_LOCAL_DIR", &d),
            ("MSYNC_DEV_MODE", "true"),
            ("MSYNC_CRA_REPO", "github:github.com:o/r"),
        ]))
        .is_err());
        let empty =
            std::env::temp_dir().join(format!("msync-local-empty-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&empty).unwrap();
        let e = empty.to_string_lossy().to_string();
        assert!(local_cra_dir_from(lookup(&[
            ("MSYNC_CRA_LOCAL_DIR", &e),
            ("MSYNC_DEV_MODE", "true")
        ]))
        .is_err());
        assert_eq!(
            local_cra_dir_from(lookup(&[("MSYNC_DEV_MODE", "true")])).unwrap(),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&empty);
    }

    #[test]
    fn dev_mode_refuses_a_live_stripe_configuration() {
        let dev = ("MSYNC_DEV_MODE", "true");
        assert!(
            check_dev_mode_stripe(lookup(&[dev, ("MSYNC_STRIPE_ALLOW_LIVE", "true")])).is_err()
        );
        assert!(
            check_dev_mode_stripe(lookup(&[dev, ("STRIPE_SECRET_KEY", "sk_live_abc")])).is_err()
        );
        assert!(
            check_dev_mode_stripe(lookup(&[dev, ("STRIPE_SECRET_KEY", " rk_live_abc")])).is_err()
        );
        assert!(
            check_dev_mode_stripe(lookup(&[dev, ("DSYNC_STRIPE_ALLOW_LIVE", "true")])).is_err(),
            "the old name counts too"
        );
        // A test key, or no key, is fine in dev mode.
        assert!(
            check_dev_mode_stripe(lookup(&[dev, ("STRIPE_SECRET_KEY", "sk_test_abc")])).is_ok()
        );
        assert!(check_dev_mode_stripe(lookup(&[dev])).is_ok());
        // Outside dev mode this check does not apply (the live guard in StripeClient does).
        assert!(check_dev_mode_stripe(lookup(&[
            ("MSYNC_STRIPE_ALLOW_LIVE", "true"),
            ("STRIPE_SECRET_KEY", "sk_live_x")
        ]))
        .is_ok());
    }

    /// Pinned (contract 12.1): the default database keeps its pre-rename file name.
    #[test]
    fn default_database_url_keeps_its_old_name() {
        assert_eq!(DEFAULT_DATABASE_URL, "sqlite://dsync.db");
    }
}
