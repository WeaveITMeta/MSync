//! Platform-neutral chat commands (contract 12.2).
//!
//! One definition of the six commands (`link`, `stats`, `leaderboard`, `sync`, `payouts`,
//! `payout-setup`) runs on Discord and on Slack. Each command takes a [`ChatActor`] (who
//! asked, on which platform) and returns a [`ChatReply`]. The Discord adapter
//! (`crate::discord`) and the Slack adapter (`crate::slack`) only translate requests in and
//! replies out; no serenity or Slack type appears in [`commands`].

pub mod commands;
pub mod identity;

#[cfg(test)]
pub(crate) mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::NaiveDate;
use sqlx::SqlitePool;

use crate::auth::Directory;
use crate::cra::loader::CraHandle;
use crate::cra::model::Identities;
use crate::payments::{PaymentConfig, StripeApi};

/// The chat platforms MSync runs on. Neither is the source of truth; both project the CRA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ChatPlatform {
    Discord,
    Slack,
}

impl ChatPlatform {
    /// Storage key in `chat_identities.platform`.
    pub fn as_str(self) -> &'static str {
        match self {
            ChatPlatform::Discord => "discord",
            ChatPlatform::Slack => "slack",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ChatPlatform::Discord => "Discord",
            ChatPlatform::Slack => "Slack",
        }
    }

    /// How a user types a command on this platform. Slack slash commands are
    /// workspace-global, so MSync's are prefixed.
    pub fn command(self, c: ChatCommand) -> String {
        match self {
            ChatPlatform::Discord => format!("/{}", c.name()),
            ChatPlatform::Slack => format!("/msync-{}", c.name()),
        }
    }

    /// The member's identity on this platform in `members.toml`.
    pub fn identity_of(self, ids: &Identities) -> Option<&str> {
        match self {
            ChatPlatform::Discord => ids.discord.as_deref(),
            ChatPlatform::Slack => ids.slack.as_deref(),
        }
    }
}

/// The six shared commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatCommand {
    Link,
    Stats,
    Leaderboard,
    Sync,
    Payouts,
    PayoutSetup,
}

impl ChatCommand {
    pub const ALL: [ChatCommand; 6] = [
        ChatCommand::Link,
        ChatCommand::Stats,
        ChatCommand::Leaderboard,
        ChatCommand::Sync,
        ChatCommand::Payouts,
        ChatCommand::PayoutSetup,
    ];

    /// The bare name (`payout-setup`). Discord registers it as is; Slack prefixes it.
    pub fn name(self) -> &'static str {
        match self {
            ChatCommand::Link => "link",
            ChatCommand::Stats => "stats",
            ChatCommand::Leaderboard => "leaderboard",
            ChatCommand::Sync => "sync",
            ChatCommand::Payouts => "payouts",
            ChatCommand::PayoutSetup => "payout-setup",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }

    /// Admin-only commands need the CRA rank or the platform admin flag.
    pub fn is_admin_only(self) -> bool {
        matches!(self, ChatCommand::Sync | ChatCommand::Payouts)
    }
}

/// Who asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatActor {
    pub platform: ChatPlatform,
    /// The Slack team id. Discord user ids are global, so the Discord adapter leaves it empty.
    pub team_id: String,
    pub user_id: String,
    /// What the platform reports: Discord's administrator permission, Slack's
    /// `users.info` `is_admin`. `None` means it could not be read, which fails closed.
    pub is_platform_admin: Option<bool>,
}

impl ChatActor {
    pub fn discord(user_id: impl Into<String>, is_platform_admin: Option<bool>) -> Self {
        ChatActor { platform: ChatPlatform::Discord, team_id: String::new(), user_id: user_id.into(), is_platform_admin }
    }

    pub fn slack(team_id: impl Into<String>, user_id: impl Into<String>, is_platform_admin: Option<bool>) -> Self {
        ChatActor { platform: ChatPlatform::Slack, team_id: team_id.into(), user_id: user_id.into(), is_platform_admin }
    }

    /// `chat_identities.team_or_guild` for this actor: the Slack team, or `''` on Discord.
    pub fn storage_team(&self) -> &str {
        match self.platform {
            ChatPlatform::Discord => "",
            ChatPlatform::Slack => self.team_id.as_str(),
        }
    }
}

/// What a command answers. `private` is ephemeral on Discord and on Slack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatReply {
    pub text: String,
    pub private: bool,
}

impl ChatReply {
    pub fn public(text: impl Into<String>) -> Self {
        ChatReply { text: text.into(), private: false }
    }
    pub fn private(text: impl Into<String>) -> Self {
        ChatReply { text: text.into(), private: true }
    }
}

/// Everything the shared commands need. Built once at startup and shared by both adapters.
pub struct ChatContext {
    pub pool: SqlitePool,
    pub payments: Arc<PaymentConfig>,
    pub stripe: Arc<dyn StripeApi>,
    /// The rank seam (owners from engine configuration, everyone else from the CRA).
    pub directory: Arc<dyn Directory>,
    pub cra: CraHandle,
    /// Fixed evaluation date (tests). Default: today in UTC.
    pub today: Option<NaiveDate>,
}

impl ChatContext {
    pub fn today(&self) -> NaiveDate {
        self.today.unwrap_or_else(|| chrono::Utc::now().date_naive())
    }
}

/// The CRA projection for one platform, as `sync` hands it to the adapter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectionPlan {
    /// Managed Discord role ids or Slack user group ids, highest rank first.
    pub managed: Vec<String>,
    /// Every `(platform user id, role or group id)` pair that should exist.
    pub assignments: BTreeSet<(String, String)>,
    /// Every CRA member's user id on this platform, including members who should hold nothing.
    pub users: BTreeSet<String>,
}

impl ProjectionPlan {
    /// The full member list of one managed role or group.
    pub fn members_of(&self, group: &str) -> Vec<String> {
        self.assignments.iter().filter(|(_, g)| g == group).map(|(u, _)| u.clone()).collect()
    }

    /// The managed roles or groups one user should hold.
    pub fn desired_for(&self, user: &str) -> BTreeSet<String> {
        self.assignments.iter().filter(|(u, _)| u == user).map(|(_, g)| g.clone()).collect()
    }

    /// `group -> members`, for every managed group.
    pub fn by_group(&self) -> BTreeMap<String, Vec<String>> {
        self.managed.iter().map(|g| (g.clone(), self.members_of(g))).collect()
    }
}

/// The outcome of applying a projection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub synced: u32,
    pub failed: u32,
    pub notes: Vec<String>,
}

/// What `sync` asks the platform to do. The Discord adapter implements it over serenity and
/// the Slack adapter over the Slack Web API.
#[async_trait]
pub trait ProjectionApplier: Send + Sync {
    /// Apply the CRA projection for this platform.
    async fn apply(&self, plan: &ProjectionPlan) -> SyncReport;
    /// No CRA active: the pre-CRA merit role sync (Discord), or nothing (Slack).
    async fn apply_legacy(&self) -> SyncReport;
}
