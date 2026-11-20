//! Accounts and sign-in (contract 13.4, hierarchy doc section 6).
//!
//! One root account per person, many identities (GitHub, GitLab, Discord, Slack, Google, email
//! link). Rank and permissions never live on an account or identity: they come from the active
//! CRA snapshot for the member the account's OAuth-proven identities name. Linking adds a way
//! in; it never adds authority.
//!
//! Browser sessions are random 256-bit tokens in a `Secure; HttpOnly; SameSite=Lax` cookie,
//! stored hashed, checked on every request. A session cookie never authorizes `/api/v1`.

pub mod providers;
pub mod routes;
pub mod rules;
pub mod store;

#[cfg(test)]
pub(crate) mod tests;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::NaiveDate;
use sqlx::SqlitePool;

pub use providers::{HttpProviderApi, ProviderApi, ProviderConfig, ProviderError, ProviderProfile};
pub use rules::{can_unlink, decide, Decision, Facts, Provider, Refusal, UnlinkRefusal};
pub use store::{chat_binding_is_verified, complete_sign_in, login_is_verified, member_for_account, AccountsError};

use crate::cra::loader::CraHandle;

/// Sends one-time sign-in links.
#[async_trait]
pub trait MailSender: Send + Sync {
    async fn send_sign_in_link(&self, to: &str, url: &str) -> Result<(), String>;
}

/// The default: no mailer is configured, so nothing is sent. It says so in the log (with the
/// address masked) and never logs the link.
pub struct NoMailer;

#[async_trait]
impl MailSender for NoMailer {
    async fn send_sign_in_link(&self, to: &str, _url: &str) -> Result<(), String> {
        tracing::warn!("no mailer is configured; the sign-in link for {} was not sent", rules::mask_email(to));
        Ok(())
    }
}

/// Everything the sign-in routes need.
pub struct AccountsContext {
    pub pool: SqlitePool,
    /// `MSYNC_PUBLIC_URL`, no trailing slash. Redirect URIs and the `Origin` check use it.
    pub public_url: String,
    pub providers: HashMap<Provider, ProviderConfig>,
    pub api: Arc<dyn ProviderApi>,
    pub mailer: Arc<dyn MailSender>,
    pub cra: CraHandle,
    /// Fixed evaluation date (tests). Default: today in UTC.
    pub today: Option<NaiveDate>,
}

impl AccountsContext {
    pub fn from_env(pool: SqlitePool, public_url: &str, cra: CraHandle) -> Self {
        AccountsContext {
            pool,
            public_url: public_url.trim_end_matches('/').to_string(),
            providers: providers::providers_from_env(),
            api: Arc::new(HttpProviderApi::new()),
            mailer: Arc::new(NoMailer),
            cra,
            today: None,
        }
    }

    pub fn today(&self) -> NaiveDate {
        self.today.unwrap_or_else(|| chrono::Utc::now().date_naive())
    }

    /// The exact redirect URI registered at the provider.
    pub fn redirect_uri(&self, p: Provider) -> String {
        format!("{}/auth/{}/callback", self.public_url, p.as_str())
    }
}

/// Test-only fault injection: `MSYNC_MUTANT=<name>` switches one deliberate bug on, so each
/// test can be shown to fail against the bug it guards. Compiled to `false` outside tests.
pub(crate) mod mutant {
    #[cfg(test)]
    pub fn on(name: &str) -> bool {
        std::env::var("MSYNC_MUTANT").map_or(false, |v| v == name)
    }

    #[cfg(not(test))]
    #[inline(always)]
    pub fn on(_name: &str) -> bool {
        false
    }
}
