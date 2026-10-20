use std::collections::HashMap;
use std::env;

/// Payment settings, all from env. Defaults are placeholders; set real values in .env.
#[derive(Debug, Clone)]
pub struct PaymentConfig {
    /// Branches whose merged PRs create ledger rows (DSYNC_PAY_BRANCHES, default "main,upcoming").
    pub branches: Vec<String>,
    /// Per-branch base amount in cents (DSYNC_PAY_BASE_CENTS_<BRANCH>).
    pub base_cents: HashMap<String, i64>,
    /// Each merit point adds this fraction of the base (DSYNC_PAY_MERIT_MULTIPLIER, default 0.01 = +1%).
    pub merit_multiplier: f64,
    /// Hard cap per ledger row in cents (DSYNC_PAY_MAX_CENTS, default 10000).
    pub max_cents: i64,
    /// ISO currency (DSYNC_PAY_CURRENCY, default "usd").
    pub currency: String,
    pub stripe_secret_key: Option<String>,
    pub stripe_webhook_secret: Option<String>,
    pub admin_token: Option<String>,
    /// Base URL used for Stripe onboarding refresh/return links (DSYNC_PUBLIC_URL).
    pub public_url: String,
}

pub const DEFAULT_BASE_CENTS: i64 = 1000;

pub fn branch_env_key(branch: &str) -> String {
    let b: String = branch
        .trim()
        .to_uppercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("DSYNC_PAY_BASE_CENTS_{}", b)
}

impl PaymentConfig {
    pub fn from_env() -> Self {
        Self::from_lookup(|k| env::var(k).ok())
    }

    pub fn from_lookup<F: Fn(&str) -> Option<String>>(get: F) -> Self {
        let non_empty = |k: &str| get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());

        let branches: Vec<String> = non_empty("DSYNC_PAY_BRANCHES")
            .unwrap_or_else(|| "main,upcoming".to_string())
            .split(',')
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty())
            .collect();

        let default_base = non_empty("DSYNC_PAY_BASE_CENTS_DEFAULT")
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_BASE_CENTS);

        let base_cents = branches
            .iter()
            .map(|b| {
                let cents = non_empty(&branch_env_key(b))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(default_base);
                (b.clone(), cents)
            })
            .collect();

        PaymentConfig {
            branches,
            base_cents,
            merit_multiplier: non_empty("DSYNC_PAY_MERIT_MULTIPLIER")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.01),
            max_cents: non_empty("DSYNC_PAY_MAX_CENTS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(10_000),
            currency: non_empty("DSYNC_PAY_CURRENCY").unwrap_or_else(|| "usd".to_string()).to_lowercase(),
            stripe_secret_key: non_empty("STRIPE_SECRET_KEY"),
            stripe_webhook_secret: non_empty("STRIPE_WEBHOOK_SECRET"),
            admin_token: non_empty("DSYNC_ADMIN_TOKEN"),
            public_url: non_empty("DSYNC_PUBLIC_URL")
                .unwrap_or_else(|| "http://localhost:8080".to_string())
                .trim_end_matches('/')
                .to_string(),
        }
    }

    pub fn is_tracked(&self, branch: &str) -> bool {
        self.branches.iter().any(|b| b == branch)
    }

    /// (base_cents, amount_cents) for a merged PR, or None if the branch is not tracked.
    /// amount = base * (1 + merit_points * merit_multiplier), rounded, clamped to [0, max_cents].
    pub fn amount_for(&self, branch: &str, merit_points: i64) -> Option<(i64, i64)> {
        if !self.is_tracked(branch) {
            return None;
        }
        let base = *self.base_cents.get(branch).unwrap_or(&DEFAULT_BASE_CENTS);
        let factor = 1.0 + (merit_points.max(0) as f64) * self.merit_multiplier;
        let amount = ((base as f64) * factor).round() as i64;
        Some((base, amount.clamp(0, self.max_cents.max(0))))
    }
}