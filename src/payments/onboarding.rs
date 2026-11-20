use super::config::PaymentConfig;
use super::ledger;
use super::signature::token_matches;
use super::stripe::StripeApi;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::SqlitePool;

#[derive(Debug, thiserror::Error)]
pub enum OnboardError {
    #[error("unknown contributor")]
    UnknownContributor,
    #[error("contributor has not linked Discord (/link)")]
    NotLinked,
    #[error("Stripe error: {0}")]
    Stripe(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

#[derive(Debug, Clone)]
pub struct OnboardLink {
    pub account_id: String,
    pub url: String,
}

/// Create (or reuse) the contributor's Express account and return a fresh onboarding link.
pub async fn onboarding_link_for_github(
    pool: &SqlitePool,
    stripe: &dyn StripeApi,
    cfg: &PaymentConfig,
    github_username: &str,
) -> Result<OnboardLink, OnboardError> {
    let row: Option<(Option<i64>, Option<String>)> =
        sqlx::query_as("SELECT discord_user_id, stripe_account_id FROM contributors WHERE github_username = ?")
            .bind(github_username)
            .fetch_optional(pool)
            .await?;
    let existing = match row {
        None => return Err(OnboardError::UnknownContributor),
        Some((None, _)) => return Err(OnboardError::NotLinked),
        Some((Some(_), acct)) => acct.filter(|a| !a.is_empty()),
    };
    account_and_link(pool, stripe, cfg, github_username, existing).await
}

/// Like [`onboarding_link_for_github`], for a chat caller whose identity the caller of this
/// function has ALREADY proven (a CRA-attested member, contract 12.2). It does not look at
/// `contributors.discord_user_id`, because a Slack-only member has none. Never call it for a
/// binding made by typing a login into `link`: that is how a payout account gets stolen.
pub async fn onboarding_link_for_verified_login(
    pool: &SqlitePool,
    stripe: &dyn StripeApi,
    cfg: &PaymentConfig,
    github_username: &str,
) -> Result<OnboardLink, OnboardError> {
    let row: Option<(Option<String>,)> = sqlx::query_as("SELECT stripe_account_id FROM contributors WHERE github_username = ?")
        .bind(github_username)
        .fetch_optional(pool)
        .await?;
    let Some((acct,)) = row else { return Err(OnboardError::UnknownContributor) };
    account_and_link(pool, stripe, cfg, github_username, acct.filter(|a| !a.is_empty())).await
}

/// Reuse `existing` or create the Express account (same idempotency key as before), then
/// issue a fresh onboarding link.
async fn account_and_link(
    pool: &SqlitePool,
    stripe: &dyn StripeApi,
    cfg: &PaymentConfig,
    github_username: &str,
    existing: Option<String>,
) -> Result<OnboardLink, OnboardError> {
    let account_id = match existing {
        Some(a) => a,
        None => {
            let a = stripe
                .create_express_account(github_username)
                .await
                .map_err(|e| OnboardError::Stripe(e.to_string()))?;
            ledger::set_stripe_account(pool, github_username, &a).await?;
            a
        }
    };
    let url = link_for_account(stripe, cfg, &account_id).await?;
    Ok(OnboardLink { account_id, url })
}

pub async fn link_for_account(stripe: &dyn StripeApi, cfg: &PaymentConfig, account_id: &str) -> Result<String, OnboardError> {
    stripe
        .create_account_link(account_id, &refresh_url(cfg, account_id), &format!("{}/payouts/return", cfg.public_url))
        .await
        .map_err(|e| OnboardError::Stripe(e.to_string()))
}

/// Refresh URL carries an HMAC (keyed by MSYNC_ADMIN_TOKEN) so only links MSync issued can be regenerated.
pub fn refresh_url(cfg: &PaymentConfig, account_id: &str) -> String {
    match refresh_sig(cfg, account_id) {
        Some(sig) => format!("{}/payouts/refresh?account={}&sig={}", cfg.public_url, account_id, sig),
        None => format!("{}/payouts/refresh", cfg.public_url),
    }
}

pub fn refresh_sig(cfg: &PaymentConfig, account_id: &str) -> Option<String> {
    let key = cfg.admin_token.as_deref()?;
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).ok()?;
    mac.update(b"payout-refresh:");
    mac.update(account_id.as_bytes());
    Some(hex::encode(mac.finalize().into_bytes()))
}

pub fn verify_refresh_sig(cfg: &PaymentConfig, account_id: &str, sig: &str) -> bool {
    refresh_sig(cfg, account_id).is_some_and(|expected| token_matches(&expected, sig))
}

/// (github_username, payouts_enabled) for a Discord user, if linked.
pub async fn contributor_for_discord(pool: &SqlitePool, discord_user_id: i64) -> Result<Option<(String, bool)>, sqlx::Error> {
    let row: Option<(String, i64)> =
        sqlx::query_as("SELECT github_username, stripe_payouts_enabled FROM contributors WHERE discord_user_id = ?")
            .bind(discord_user_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(g, e)| (g, e != 0)))
}