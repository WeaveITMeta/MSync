//! `msync serve`: the HTTP service, the chat bots and the background timers.
//!
//! One command starts the service with only a database (contract 13.5). Discord, Slack,
//! Stripe, GitHub, GitLab and every OAuth provider are optional; each one that is not
//! configured is logged and skipped. `MSYNC_DEV_MODE=true` additionally allows no chat
//! platform at all and a local CRA directory (`MSYNC_CRA_LOCAL_DIR`).

use std::str::FromStr;
use std::sync::Arc;

use actix_web::{middleware, web, App, HttpServer};
use anyhow::{Context, Result};
use serenity::prelude::*;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::auth::{CraDirectory, Directory, EnvDirectory};
use crate::config::Config;
use crate::discord::{self, bot::DiscordBot};
use crate::mcp;
use crate::merit::api as merit_api;
use crate::merit::tracker::MeritTracker;
use crate::payments::{self, PaymentConfig, StripeClient};
use crate::AppState;

/// Open the database, creating the SQLite file when it does not exist yet, and run the
/// migrations. A fresh install has no database file; `SqlitePool::connect` alone refuses
/// to open a missing file ("unable to open database file").
pub async fn open_database(url: &str) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(url)
        .with_context(|| format!("DATABASE_URL is not a valid SQLite URL: {url}"))?
        .create_if_missing(true)
        .busy_timeout(std::time::Duration::from_secs(10));
    let pool = SqlitePoolOptions::new()
        .connect_with(opts)
        .await
        .with_context(|| format!("could not open the database {url}"))?;
    sqlx::migrate!("./migrations").run(&pool).await.context("database migrations failed")?;
    Ok(pool)
}

/// `MSYNC_OWNER_MEMBER_IDS` in the order written (the first `provider:login` entry is the
/// actor of a local CRA load).
pub fn owner_list_in_order() -> Vec<String> {
    crate::config::env_var("MSYNC_OWNER_MEMBER_IDS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Every route the service registers. `msync serve` and the route tests (OpenAPI drift)
/// call this one function, so they cannot disagree.
pub fn configure_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(crate::routes::health_check)
        .service(crate::routes::github_webhook)
        .service(crate::routes::gitlab_webhook)
        .service(crate::slack::slack_commands)
        .service(crate::routes::get_contributor_stats)
        .service(crate::routes::get_leaderboard)
        .service(payments::routes::onboard)
        .service(payments::routes::stripe_webhook)
        .service(payments::routes::payouts_return)
        .service(payments::routes::payouts_refresh)
        .service(payments::routes::list_ledger)
        .service(payments::routes::approve_entry)
        .service(payments::routes::reject_entry)
        .configure(crate::cra::api::configure) // before payments: guards PUT /payouts/policy
        .configure(payments::api::configure)
        .configure(merit_api::configure)
        .configure(mcp::configure)
        .configure(crate::audit::api::configure)
        .configure(crate::portal::configure)
        .configure(crate::accounts::routes::configure)
        .configure(crate::oauth::configure);
}

pub async fn serve() -> Result<()> {
    info!("Starting MSync (MeritSync) - merit-based communities on Discord and Slack");

    // Load configuration
    let config = Config::from_env()?;
    if config.dev_mode {
        warn!("==================================================================");
        warn!("MSYNC_DEV_MODE=true: DEVELOPMENT MODE. Not safe on a reachable host.");
        warn!("Chat platforms are optional, a local CRA directory is allowed, and");
        warn!("/health reports dev_mode. Live Stripe configuration is refused.");
        warn!("==================================================================");
    }

    // Initialize database (created when missing) and run migrations
    let db_pool = open_database(&config.database_url).await?;
    info!("Database migrations completed");

    // Initialize merit tracker
    let merit_tracker = Arc::new(RwLock::new(MeritTracker::new(db_pool.clone())));

    // Payment model (ledger + Stripe Connect). Transfers only happen on admin approve.
    let payment_config = Arc::new(PaymentConfig::from_env());
    let allow_live = crate::config::env_var("MSYNC_STRIPE_ALLOW_LIVE").map(|v| v == "true").unwrap_or(false);
    let stripe_client = StripeClient::new(payment_config.stripe_secret_key.clone(), allow_live);
    if !stripe_client.is_configured() {
        info!("Stripe not configured; payout approvals will fail until STRIPE_SECRET_KEY is set");
    }
    info!("Tracked payout branches: {:?}", payment_config.branches);

    let stripe_api: Arc<dyn payments::StripeApi> = Arc::new(stripe_client);
    let payment_config_public_url = payment_config.public_url.clone();

    // Discord HTTP client for webhook use. Without Discord it is never called.
    let discord_http = Arc::new(serenity::http::Http::new(
        config.discord.as_ref().map(|d| d.token.as_str()).unwrap_or(""),
    ));

    // API tokens rank seam and payout controls (v1 API + scheduler)
    let env_directory = EnvDirectory::from_env();
    info!("Payout approver owners configured: {}", env_directory.owner_count());
    // CRA: the active snapshot (if any) from the store; refuse to start on an unusable one.
    let cra_owners = env_directory.owner_entries();
    let cra_handle = crate::cra::CraHandle::new();
    cra_handle
        .reload_from_store(&db_pool, crate::cra::loader::ORG_ID)
        .await
        .map_err(anyhow::Error::msg)?;
    let directory: Arc<dyn Directory> = Arc::new(CraDirectory::new(env_directory, cra_handle.clone()));
    // The CRA repository (MSYNC_CRA_REPO), or in dev mode a local directory standing in for it.
    let local = crate::cra::loader::local_from_config(&config, &owner_list_in_order()).map_err(anyhow::Error::msg)?;
    let is_local = local.is_some();
    let (cra_settings, cra_source) = match local {
        Some((settings, source)) => (Some(settings), Some(source)),
        None => (
            crate::cra::loader::settings_from_config(&config),
            crate::cra::loader::source_from_config(&config),
        ),
    };
    let cra_ctx = Arc::new(crate::cra::CraContext::new(
        db_pool.clone(),
        cra_handle.clone(),
        directory.clone(),
        cra_owners,
        cra_settings,
        cra_source,
    ));
    if is_local {
        warn!(
            "CRA from a LOCAL DIRECTORY {:?} (dev mode); loaded as owner {:?}. POST /api/v1/cra/reload rereads it.",
            config.cra.local_dir,
            crate::cra::local::local_actor(&owner_list_in_order()).map(|a| a.to_string())
        );
        match cra_ctx.reload_head(chrono::Utc::now().date_naive()).await {
            Ok(outcome) => match &outcome {
                crate::cra::LoadOutcome::Rejected { violations, .. } => {
                    for v in violations {
                        error!("local CRA rejected: {}: {}: {}", v.code, v.item, v.message);
                    }
                    error!("the local CRA was NOT loaded; fix it and POST /api/v1/cra/reload");
                }
                other => info!("local CRA load: {}", other.label()),
            },
            Err(e) => error!("local CRA load failed: {}", e),
        }
    }
    match (&cra_ctx.settings, cra_handle.active()) {
        (Some(s), active) => info!(
            "CRA repository {} branch {}; active snapshot: {:?}",
            s.repo.spec(),
            s.branch,
            active.map(|a| a.commit_sha.clone())
        ),
        (None, _) => info!("No CRA repository configured (MSYNC_CRA_REPO); ranks from MSYNC_OWNER_MEMBER_IDS only"),
    }
    tokio::spawn(crate::cra::loader::run_merit_refresher(cra_ctx.clone(), std::time::Duration::from_secs(60)));

    // The shared chat commands (contract 12.2): one context for the Discord bot and the Slack app.
    let chat_ctx = Arc::new(crate::chat::ChatContext {
        pool: db_pool.clone(),
        payments: payment_config.clone(),
        stripe: stripe_api.clone(),
        directory: directory.clone(),
        cra: cra_handle.clone(),
        today: None,
    });

    // Role sync (contract 13.3): Discord roles and Slack user groups are projections of the
    // CRA. Each platform has one runner; triggers are snapshot activation, member join,
    // member update (Discord), the `sync` command and a timer (MSYNC_ROLE_SYNC_SECS, default
    // 300, 0 disables). A platform that is not configured is skipped.
    let role_sync_every = crate::chat::sync::interval_from_env();
    info!("Role sync timer: {:?}", role_sync_every);
    let discord_sync = config.discord.as_ref().map(|_| {
        let platform = crate::discord::sync::DiscordPlatform {
            http: discord_http.clone(),
            cra: cra_handle.clone(),
            fallback_guild: std::env::var("DISCORD_GUILD_ID").ok().and_then(|v| v.trim().parse::<u64>().ok()),
            token_set: true,
        };
        let runner = Arc::new(crate::chat::sync::SyncRunner::new(Arc::new(platform), db_pool.clone(), cra_handle.clone()));
        crate::chat::sync::spawn(runner, role_sync_every)
    });

    // Discord: started only when DISCORD_TOKEN is set.
    let discord_handle = match config.discord.clone() {
        Some(dc) => {
            let bot = DiscordBot::new(db_pool.clone(), merit_tracker.clone(), chat_ctx.clone(), discord_sync.clone());
            Some(tokio::spawn(async move {
                let intents = GatewayIntents::GUILDS
                    | GatewayIntents::GUILD_MEMBERS
                    | GatewayIntents::GUILD_MESSAGES
                    | GatewayIntents::MESSAGE_CONTENT;

                let mut client = match Client::builder(&dc.token, intents)
                    .application_id(dc.application_id.into())
                    .event_handler(bot)
                    .framework(discord::framework::get_framework())
                    .await
                {
                    Ok(c) => c,
                    Err(e) => {
                        error!("Failed to create the Discord client: {:?}", e);
                        return;
                    }
                };

                if let Err(why) = client.start().await {
                    error!("Discord client error: {:?}", why);
                }
            }))
        }
        None => {
            info!("Discord not configured (DISCORD_TOKEN unset); running without the Discord bot");
            None
        }
    };

    // Slack: POST /slack/commands. Enabled only when MSYNC_SLACK_SIGNING_SECRET is set; with
    // no secret the route rejects every request.
    let slack_api: Arc<dyn crate::slack::SlackApi> = Arc::new(crate::slack::SlackWebApi::new(
        config.slack.api_base.clone(),
        config.slack.bot_token.clone(),
    ));
    let slack_runner = Arc::new(crate::chat::sync::SyncRunner::new(
        Arc::new(crate::slack::sync::SlackPlatform::new(slack_api.clone(), cra_handle.clone())),
        db_pool.clone(),
        cra_handle.clone(),
    ));
    if config.slack.bot_token.is_some() {
        crate::chat::sync::spawn(slack_runner.clone(), role_sync_every);
    }
    let slack_state = crate::slack::SlackState {
        chat: chat_ctx.clone(),
        signing_secret: config.slack.signing_secret.clone(),
        team_id: config.slack.team_id.clone(),
        api: slack_api,
        sync: slack_runner,
    };
    if slack_state.enabled() {
        info!("Slack enabled: POST /slack/commands (team {:?} plus the CRA's [slack] team)", config.slack.team_id);
        if config.slack.bot_token.is_none() {
            info!("MSYNC_SLACK_BOT_TOKEN unset: Slack admin checks fail closed and /msync-sync cannot update user groups");
        }
    } else {
        info!("Slack not configured (MSYNC_SLACK_SIGNING_SECRET unset); /slack/commands rejects every request");
    }
    let slack_data = web::Data::new(slack_state);
    let api_ctx = payments::api::ApiContext {
        pool: db_pool.clone(),
        payments: payment_config.clone(),
        stripe: stripe_api.clone(),
        directory: directory.clone(),
    };
    let scheduler_secs: u64 = crate::config::env_var("MSYNC_PAYOUT_SCHEDULER_SECS").and_then(|v| v.parse().ok()).unwrap_or(60);
    if scheduler_secs > 0 {
        tokio::spawn(payments::schedule::run_scheduler(api_ctx.clone(), std::time::Duration::from_secs(scheduler_secs)));
    } else {
        info!("Payout scheduler disabled (MSYNC_PAYOUT_SCHEDULER_SECS=0)");
    }
    let mcp_api_ctx = api_ctx.clone();
    let api_data = web::Data::new(api_ctx);

    // Create app state
    let app_state = web::Data::new(AppState {
        config: config.clone(),
        db_pool: db_pool.clone(),
        merit_tracker: merit_tracker.clone(),
        discord_http,
        payments: payment_config,
        stripe: stripe_api,
        directory,
        cra: cra_ctx.clone(),
    });
    let cra_data = web::Data::from(cra_ctx);

    // Portal sync API (contract 13.2): consumers from the active CRA, secrets from
    // MSYNC_CONSUMER_SECRET_<KEY>_<KEYID>, pairwise subjects from MSYNC_SUBJECT_KEY.
    let portal_data = web::Data::new(crate::portal::PortalContext::from_env(db_pool.clone(), cra_handle.clone()));
    info!("Portal sync API: subject key configured: {}", portal_data.subject_key_configured());

    // v1 merit/leaderboard REST and the MCP server share one MeritContext. Registry and rules
    // are read per request through the routes seams (foundry_registry, merit_rules).
    let merit_ctx = merit_api::MeritContext {
        pool: db_pool.clone(),
        seams: Arc::new(merit_api::AppStateSeams(app_state.clone())),
    };
    let allow_mcp_execute = mcp::McpContext::allow_execute_from_env();
    info!("MCP payouts_run_execute enabled: {}", allow_mcp_execute);
    let mcp_data = web::Data::new(mcp::McpContext::new(mcp_api_ctx, merit_ctx.clone(), allow_mcp_execute));
    let merit_data = web::Data::new(merit_ctx);

    // Accounts and sign-in (contract 13.4), and MSync as an OAuth/OIDC provider (12.3).
    let accounts_ctx =
        crate::accounts::AccountsContext::from_env(db_pool.clone(), &payment_config_public_url, cra_handle.clone());
    let mut sign_in: Vec<crate::accounts::Provider> = accounts_ctx.providers.keys().copied().collect();
    sign_in.sort_by_key(|p| p.as_str());
    if sign_in.is_empty() {
        info!("No sign-in provider enabled (set MSYNC_OAUTH_<PROVIDER>_CLIENT_ID and _CLIENT_SECRET; docs/SIGN_IN.md)");
    } else {
        info!("Sign-in providers: {:?}", sign_in.iter().map(|p| p.as_str()).collect::<Vec<_>>());
    }
    let oauth_ctx = crate::oauth::OAuthContext::from_env(db_pool.clone(), &payment_config_public_url, cra_handle.clone(), sign_in);
    if oauth_ctx.kek.is_none() {
        info!("MSync OAuth provider disabled (MSYNC_KEY_ENCRYPTION_KEY unset); /oauth/* answers 503");
    }
    let accounts_data = web::Data::new(accounts_ctx);
    let oauth_data = web::Data::new(oauth_ctx);

    // Start Actix web server
    info!("Starting web server on {}:{}", config.server.host, config.server.port);

    HttpServer::new(move || {
        App::new()
            .app_data(app_state.clone())
            .app_data(api_data.clone())
            .app_data(cra_data.clone())
            .app_data(merit_data.clone())
            .app_data(mcp_data.clone())
            .app_data(slack_data.clone())
            .app_data(portal_data.clone())
            .app_data(accounts_data.clone())
            .app_data(oauth_data.clone())
            .wrap(middleware::Logger::default())
            .wrap(middleware::NormalizePath::trim())
            .configure(configure_routes)
    })
    .bind((config.server.host.as_str(), config.server.port))
    .with_context(|| format!("could not listen on {}:{}", config.server.host, config.server.port))?
    .run()
    .await?;

    // Wait for Discord bot to finish
    if let Some(h) = discord_handle {
        h.await?;
    }

    Ok(())
}
