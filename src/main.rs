use actix_web::{middleware, web, App, HttpServer};
use anyhow::Result;
use dotenv::dotenv;
use serenity::prelude::*;
use sqlx::sqlite::SqlitePool;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use msync::auth::{CraDirectory, Directory, EnvDirectory};
use msync::config::Config;
use msync::discord::{self, bot::DiscordBot};
use msync::mcp;
use msync::merit::api as merit_api;
use msync::merit::tracker::MeritTracker;
use msync::payments::{self, PaymentConfig, StripeClient};
use msync::routes;
use msync::AppState;


#[actix_web::main]
async fn main() -> Result<()> {
    // Load environment variables
    dotenv().ok();

    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "msync=debug,actix_web=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    info!("Starting MSync (MeritSync) - merit-based communities on Discord and Slack");

    // Load configuration
    let config = Config::from_env()?;

    // Initialize database
    let db_pool = SqlitePool::connect(&config.database_url).await?;

    // Run migrations
    sqlx::migrate!("./migrations").run(&db_pool).await?;
    info!("Database migrations completed");

    // Initialize merit tracker
    let merit_tracker = Arc::new(RwLock::new(MeritTracker::new(db_pool.clone())));

    // Payment model (ledger + Stripe Connect). Transfers only happen on admin approve.
    let payment_config = Arc::new(PaymentConfig::from_env());
    let allow_live = msync::config::env_var("MSYNC_STRIPE_ALLOW_LIVE").map(|v| v == "true").unwrap_or(false);
    let stripe_client = StripeClient::new(payment_config.stripe_secret_key.clone(), allow_live);
    if !stripe_client.is_configured() {
        info!("Stripe not configured; payout approvals will fail until STRIPE_SECRET_KEY is set");
    }
    info!("Tracked payout branches: {:?}", payment_config.branches);

    let stripe_api: Arc<dyn payments::StripeApi> = Arc::new(stripe_client);

    // Discord HTTP client for webhook use. Without Discord it is never called.
    let discord_http = Arc::new(serenity::http::Http::new(
        config.discord.as_ref().map(|d| d.token.as_str()).unwrap_or(""),
    ));

    // API tokens rank seam and payout controls (v1 API + scheduler)
    let env_directory = EnvDirectory::from_env();
    info!("Payout approver owners configured: {}", env_directory.owner_count());
    // CRA: the active snapshot (if any) from the store; refuse to start on an unusable one.
    let cra_owners = env_directory.owner_entries();
    let cra_handle = msync::cra::CraHandle::new();
    cra_handle
        .reload_from_store(&db_pool, msync::cra::loader::ORG_ID)
        .await
        .map_err(anyhow::Error::msg)?;
    let directory: Arc<dyn Directory> = Arc::new(CraDirectory::new(env_directory, cra_handle.clone()));
    let cra_ctx = Arc::new(msync::cra::CraContext::new(
        db_pool.clone(),
        cra_handle.clone(),
        directory.clone(),
        cra_owners,
        msync::cra::loader::settings_from_config(&config),
        msync::cra::loader::source_from_config(&config),
    ));
    match (&cra_ctx.settings, cra_handle.active()) {
        (Some(s), active) => info!(
            "CRA repository {} branch {}; active snapshot: {:?}",
            s.repo.spec(),
            s.branch,
            active.map(|a| a.commit_sha.clone())
        ),
        (None, _) => info!("No CRA repository configured (MSYNC_CRA_REPO); ranks from MSYNC_OWNER_MEMBER_IDS only"),
    }
    tokio::spawn(msync::cra::loader::run_merit_refresher(cra_ctx.clone(), std::time::Duration::from_secs(60)));

    // The shared chat commands (contract 12.2): one context for the Discord bot and the Slack app.
    let chat_ctx = Arc::new(msync::chat::ChatContext {
        pool: db_pool.clone(),
        payments: payment_config.clone(),
        stripe: stripe_api.clone(),
        directory: directory.clone(),
        cra: cra_handle.clone(),
        today: None,
    });

    // Discord: started only when DISCORD_TOKEN is set.
    let discord_handle = match config.discord.clone() {
        Some(dc) => {
            let bot = DiscordBot::new(db_pool.clone(), merit_tracker.clone(), chat_ctx.clone());
            Some(tokio::spawn(async move {
                let intents = GatewayIntents::GUILDS
                    | GatewayIntents::GUILD_MEMBERS
                    | GatewayIntents::GUILD_MESSAGES
                    | GatewayIntents::MESSAGE_CONTENT;

                let mut client = Client::builder(&dc.token, intents)
                    .application_id(dc.application_id.into())
                    .event_handler(bot)
                    .framework(discord::framework::get_framework())
                    .await
                    .expect("Failed to create Discord client");

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
    let slack_api: Arc<dyn msync::slack::SlackApi> = Arc::new(msync::slack::SlackWebApi::new(
        config.slack.api_base.clone(),
        config.slack.bot_token.clone(),
    ));
    let slack_state = msync::slack::SlackState {
        chat: chat_ctx.clone(),
        signing_secret: config.slack.signing_secret.clone(),
        team_id: config.slack.team_id.clone(),
        api: slack_api,
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
    let scheduler_secs: u64 = msync::config::env_var("MSYNC_PAYOUT_SCHEDULER_SECS").and_then(|v| v.parse().ok()).unwrap_or(60);
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
            .wrap(middleware::Logger::default())
            .wrap(middleware::NormalizePath::trim())
            .service(routes::health_check)
            .service(routes::github_webhook)
            .service(routes::gitlab_webhook)
            .service(msync::slack::slack_commands)
            .service(routes::get_contributor_stats)
            .service(routes::get_leaderboard)
            .service(payments::routes::onboard)
            .service(payments::routes::stripe_webhook)
            .service(payments::routes::payouts_return)
            .service(payments::routes::payouts_refresh)
            .service(payments::routes::list_ledger)
            .service(payments::routes::approve_entry)
            .service(payments::routes::reject_entry)
            .configure(msync::cra::api::configure) // before payments: guards PUT /payouts/policy
            .configure(payments::api::configure)
            .configure(merit_api::configure)
            .configure(mcp::configure)
    })
    .bind((config.server.host.as_str(), config.server.port))?
    .run()
    .await?;

    // Wait for Discord bot to finish
    if let Some(h) = discord_handle {
        h.await?;
    }

    Ok(())
}
