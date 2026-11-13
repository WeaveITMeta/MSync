use actix_web::{middleware, web, App, HttpServer};
use anyhow::Result;
use dotenv::dotenv;
use serenity::prelude::*;
use sqlx::sqlite::SqlitePool;
use std::env;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use dsync::auth::{CraDirectory, Directory, EnvDirectory};
use dsync::config::Config;
use dsync::discord::{self, bot::DiscordBot};
use dsync::mcp;
use dsync::merit::api as merit_api;
use dsync::merit::tracker::MeritTracker;
use dsync::payments::{self, PaymentConfig, StripeClient};
use dsync::routes;
use dsync::AppState;


#[actix_web::main]
async fn main() -> Result<()> {
    // Load environment variables
    dotenv().ok();

    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dsync=debug,actix_web=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    info!("Starting DSync - Discord Bot for Merit-Based Open Source Communities");

    // Load configuration
    let config = Config::from_env()?;

    // Initialize database
    let db_pool = SqlitePool::connect(&config.database_url).await?;
    
    // Run migrations
    sqlx::migrate!("./migrations").run(&db_pool).await?;
    info!("Database migrations completed");

    // Initialize merit tracker
    let merit_tracker = Arc::new(RwLock::new(MeritTracker::new(db_pool.clone())));

    // Clone values for Discord bot
    let discord_token = config.discord.token.clone();
    let discord_app_id = config.discord.application_id;
    let discord_db_pool = db_pool.clone();
    let discord_merit_tracker = merit_tracker.clone();

    // Payment model (ledger + Stripe Connect). Transfers only happen on admin approve.
    let payment_config = Arc::new(PaymentConfig::from_env());
    let allow_live = env::var("DSYNC_STRIPE_ALLOW_LIVE").map(|v| v == "true").unwrap_or(false);
    let stripe_client = StripeClient::new(payment_config.stripe_secret_key.clone(), allow_live);
    if !stripe_client.is_configured() {
        info!("Stripe not configured; payout approvals will fail until STRIPE_SECRET_KEY is set");
    }
    info!("Tracked payout branches: {:?}", payment_config.branches);

    let stripe_api: Arc<dyn payments::StripeApi> = Arc::new(stripe_client);
    let discord_payments = payment_config.clone();
    let discord_stripe = stripe_api.clone();

    // Initialize Discord HTTP client for webhook use
    let discord_http = Arc::new(serenity::http::Http::new(&discord_token));

    // Start Discord bot in a separate task
    let discord_handle = tokio::spawn(async move {
        let intents = GatewayIntents::GUILDS
            | GatewayIntents::GUILD_MEMBERS
            | GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT;

        let bot = DiscordBot::new(discord_db_pool, discord_merit_tracker, discord_payments, discord_stripe);
        
        let mut client = Client::builder(&discord_token, intents)
            .application_id(discord_app_id.into())
            .event_handler(bot)
            .framework(discord::framework::get_framework())
            .await
            .expect("Failed to create Discord client");

        if let Err(why) = client.start().await {
            error!("Discord client error: {:?}", why);
        }
    });

    // API tokens rank seam and payout controls (v1 API + scheduler)
    let env_directory = EnvDirectory::from_env();
    info!("Payout approver owners configured: {}", env_directory.owner_count());
    // CRA: the active snapshot (if any) from the store; refuse to start on an unusable one.
    let cra_owners = env_directory.owner_entries();
    let cra_handle = dsync::cra::CraHandle::new();
    cra_handle
        .reload_from_store(&db_pool, dsync::cra::loader::ORG_ID)
        .await
        .map_err(anyhow::Error::msg)?;
    let directory: Arc<dyn Directory> = Arc::new(CraDirectory::new(env_directory, cra_handle.clone()));
    let cra_ctx = Arc::new(dsync::cra::CraContext::new(
        db_pool.clone(),
        cra_handle.clone(),
        directory.clone(),
        cra_owners,
        dsync::cra::loader::settings_from_config(&config),
        dsync::cra::loader::source_from_config(&config),
    ));
    match (&cra_ctx.settings, cra_handle.active()) {
        (Some(s), active) => info!(
            "CRA repository {} branch {}; active snapshot: {:?}",
            s.repo.spec(),
            s.branch,
            active.map(|a| a.commit_sha.clone())
        ),
        (None, _) => info!("No CRA repository configured (DSYNC_CRA_REPO); ranks from DSYNC_OWNER_MEMBER_IDS only"),
    }
    tokio::spawn(dsync::cra::loader::run_merit_refresher(cra_ctx.clone(), std::time::Duration::from_secs(60)));
    let api_ctx = payments::api::ApiContext {
        pool: db_pool.clone(),
        payments: payment_config.clone(),
        stripe: stripe_api.clone(),
        directory: directory.clone(),
    };
    let scheduler_secs: u64 = env::var("DSYNC_PAYOUT_SCHEDULER_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
    if scheduler_secs > 0 {
        tokio::spawn(payments::schedule::run_scheduler(api_ctx.clone(), std::time::Duration::from_secs(scheduler_secs)));
    } else {
        info!("Payout scheduler disabled (DSYNC_PAYOUT_SCHEDULER_SECS=0)");
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
    // are read per request through the routes seams (forge_registry, merit_rules).
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
            .wrap(middleware::Logger::default())
            .wrap(middleware::NormalizePath::trim())
            .service(routes::health_check)
            .service(routes::github_webhook)
            .service(routes::gitlab_webhook)
            .service(routes::get_contributor_stats)
            .service(routes::get_leaderboard)
            .service(payments::routes::onboard)
            .service(payments::routes::stripe_webhook)
            .service(payments::routes::payouts_return)
            .service(payments::routes::payouts_refresh)
            .service(payments::routes::list_ledger)
            .service(payments::routes::approve_entry)
            .service(payments::routes::reject_entry)
            .configure(dsync::cra::api::configure) // before payments: guards PUT /payouts/policy
            .configure(payments::api::configure)
            .configure(merit_api::configure)
            .configure(mcp::configure)
    })
    .bind((config.server.host.as_str(), config.server.port))?
    .run()
    .await?;

    // Wait for Discord bot to finish
    discord_handle.await?;

    Ok(())
}
