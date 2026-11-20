use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::Value;
use tracing::warn;

#[derive(Debug, Clone)]
pub struct TransferRequest {
    pub amount_cents: i64,
    pub currency: String,
    pub destination: String,
    pub idempotency_key: String,
    pub ledger_id: String,
    pub pr_number: i64,
}

/// Stripe idempotency key for creating a contributor's Express account.
///
/// Keeps its pre-rename name on purpose, for the same reason as the ledger key (contract
/// 12.1): an account creation retried across the MSync rename must present the same key, or
/// Stripe could open a second account for the same contributor.
pub fn account_idempotency_key(github_username: &str) -> String {
    format!("dsync-account-{}", github_username)
}

/// The Stripe operations MSync uses. Mocked in tests.
#[async_trait]
pub trait StripeApi: Send + Sync {
    async fn create_express_account(&self, github_username: &str) -> Result<String>;
    async fn create_account_link(
        &self,
        account_id: &str,
        refresh_url: &str,
        return_url: &str,
    ) -> Result<String>;
    async fn create_transfer(&self, req: &TransferRequest) -> Result<String>;
}

/// Minimal Stripe REST client (form-encoded, https://api.stripe.com/v1).
/// Refuses live keys (sk_live_/rk_live_) unless `allow_live` is set.
pub struct StripeClient {
    secret_key: Option<String>,
    http: reqwest::Client,
    base_url: String,
}

impl StripeClient {
    pub fn new(secret_key: Option<String>, allow_live: bool) -> Self {
        let secret_key = match secret_key {
            Some(k) if k.contains("_live_") && !allow_live => {
                warn!("Live Stripe key detected but MSYNC_STRIPE_ALLOW_LIVE is not true; Stripe disabled");
                None
            }
            other => other,
        };
        Self {
            secret_key,
            http: reqwest::Client::new(),
            base_url: "https://api.stripe.com/v1".to_string(),
        }
    }

    pub fn is_configured(&self) -> bool {
        self.secret_key.is_some()
    }

    async fn post(
        &self,
        path: &str,
        params: &[(String, String)],
        idempotency_key: Option<&str>,
    ) -> Result<Value> {
        let key = self
            .secret_key
            .as_deref()
            .ok_or_else(|| anyhow!("Stripe is not configured (STRIPE_SECRET_KEY missing)"))?;
        let mut rb = self
            .http
            .post(format!("{}{}", self.base_url, path))
            .bearer_auth(key)
            .form(params);
        if let Some(k) = idempotency_key {
            rb = rb.header("Idempotency-Key", k);
        }
        let resp = rb.send().await?;
        let status = resp.status();
        let body: Value = resp.json().await?;
        if !status.is_success() {
            bail!(
                "Stripe error {}: {}",
                status,
                body["error"]["message"].as_str().unwrap_or("unknown error")
            );
        }
        Ok(body)
    }
}

fn p(k: &str, v: impl ToString) -> (String, String) {
    (k.to_string(), v.to_string())
}

#[async_trait]
impl StripeApi for StripeClient {
    async fn create_express_account(&self, github_username: &str) -> Result<String> {
        let v = self
            .post(
                "/accounts",
                &[
                    p("type", "express"),
                    p("capabilities[transfers][requested]", "true"),
                    p("metadata[github_username]", github_username),
                ],
                Some(&account_idempotency_key(github_username)),
            )
            .await?;
        v["id"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| anyhow!("Stripe account response missing id"))
    }

    async fn create_account_link(
        &self,
        account_id: &str,
        refresh_url: &str,
        return_url: &str,
    ) -> Result<String> {
        let v = self
            .post(
                "/account_links",
                &[
                    p("account", account_id),
                    p("refresh_url", refresh_url),
                    p("return_url", return_url),
                    p("type", "account_onboarding"),
                ],
                None,
            )
            .await?;
        v["url"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| anyhow!("Stripe account link response missing url"))
    }

    async fn create_transfer(&self, req: &TransferRequest) -> Result<String> {
        let v = self
            .post(
                "/transfers",
                &[
                    p("amount", req.amount_cents),
                    p("currency", &req.currency),
                    p("destination", &req.destination),
                    p("metadata[ledger_id]", &req.ledger_id),
                    p("metadata[pr_number]", req.pr_number),
                ],
                Some(&req.idempotency_key),
            )
            .await?;
        v["id"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| anyhow!("Stripe transfer response missing id"))
    }
}
