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

use dsync::config::Config;
use dsync::discord::{self, bot::DiscordBot};
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

    // Initialize Discord HTTP client for webhook use
    let discord_http = Arc::new(serenity::http::Http::new(&discord_token));

    // Start Discord bot in a separate task
    let discord_handle = tokio::spawn(async move {
        let intents = GatewayIntents::GUILDS
            | GatewayIntents::GUILD_MEMBERS
            | GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT;

        let bot = DiscordBot::new(discord_db_pool, discord_merit_tracker);
        
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

    // Payment model (ledger + Stripe Connect). Transfers only happen on admin approve.
    let payment_config = Arc::new(PaymentConfig::from_env());
    let allow_live = env::var("DSYNC_STRIPE_ALLOW_LIVE").map(|v| v == "true").unwrap_or(false);
    let stripe_client = StripeClient::new(payment_config.stripe_secret_key.clone(), allow_live);
    if !stripe_client.is_configured() {
        info!("Stripe not configured; payout approvals will fail until STRIPE_SECRET_KEY is set");
    }
    info!("Tracked payout branches: {:?}", payment_config.branches);

    // Create app state
    let app_state = web::Data::new(AppState {
        config: config.clone(),
        db_pool: db_pool.clone(),
        merit_tracker: merit_tracker.clone(),
        discord_http,
        payments: payment_config,
        stripe: Arc::new(stripe_client),
    });

    // Start Actix web server
    info!("Starting web server on {}:{}", config.server.host, config.server.port);
    
    HttpServer::new(move || {
        App::new()
            .app_data(app_state.clone())
            .wrap(middleware::Logger::default())
            .wrap(middleware::NormalizePath::trim())
            .service(routes::health_check)
            .service(routes::github_webhook)
            .service(routes::get_contributor_stats)
            .service(routes::get_leaderboard)
            .service(payments::routes::onboard)
            .service(payments::routes::stripe_webhook)
            .service(payments::routes::list_ledger)
            .service(payments::routes::approve_entry)
            .service(payments::routes::reject_entry)
    })
    .bind((config.server.host.as_str(), config.server.port))?
    .run()
    .await?;

    // Wait for Discord bot to finish
    discord_handle.await?;

    Ok(())
}
