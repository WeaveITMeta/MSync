//! Serde model of the five CRA files (contract section 3, schema 1).
//!
//! Every struct rejects unknown keys (`deny_unknown_fields`): a typo or a key
//! from a newer schema fails closed instead of being silently ignored.

use std::collections::BTreeMap;

use chrono::NaiveDate;
use serde::{Deserialize, Deserializer, Serialize};

use super::catalog::PermissionClass;
use super::policy::Mode;

// ---------------------------------------------------------------- org.toml

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrgFile {
    pub schema: u32,
    pub org: OrgInfo,
    /// Overrides of the mode defaults, keyed by dotted policy key.
    #[serde(default)]
    pub policy: BTreeMap<String, toml::Value>,
    /// Permissions beyond the built-in catalog.
    #[serde(default)]
    pub permissions_extra: Vec<ExtraPermission>,
    /// Optional Discord guild. At least one of `[discord]` and `[slack]` is present.
    pub discord: Option<DiscordConfig>,
    /// Optional Slack workspace. At least one of `[discord]` and `[slack]` is present.
    pub slack: Option<SlackConfig>,
    #[serde(default, rename = "foundry")]
    pub foundries: Vec<FoundryRepo>,
    #[serde(default, rename = "consumer")]
    pub consumers: Vec<Consumer>,
    /// Need-to-know compartments that roles may reference.
    #[serde(default, rename = "compartment")]
    pub compartments: Vec<Compartment>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrgInfo {
    pub slug: String,
    pub name: String,
    pub mode: Mode,
    /// Informational only.
    pub template: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtraPermission {
    pub key: String,
    pub class: PermissionClass,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscordConfig {
    pub guild_id: String,
    pub alert_channel_id: Option<String>,
}

/// The Slack workspace an organization uses (contract 12.2). Slack ids are
/// uppercase letters and digits: a team id starts with `T` (or `E` for an
/// Enterprise Grid org), a channel id with `C` or `G`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlackConfig {
    pub team_id: String,
    pub alert_channel_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FoundryProvider {
    Github,
    Gitlab,
}

impl FoundryProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            FoundryProvider::Github => "github",
            FoundryProvider::Gitlab => "gitlab",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepoVisibility {
    Public,
    Private,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FoundryRepo {
    pub provider: FoundryProvider,
    /// Optional; a self-managed GitLab host goes here.
    pub host: Option<String>,
    pub repo: String,
    pub visibility: RepoVisibility,
    /// Required. A mirror sets false and earns nothing.
    pub canonical: bool,
    #[serde(default)]
    pub payout_branches: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Consumer {
    pub key: String,
    pub label: String,
    pub scopes: Vec<String>,
    pub fields: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Classification {
    Internal,
    Confidential,
    Restricted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compartment {
    pub key: String,
    pub label: String,
    pub classification: Classification,
}

// -------------------------------------------------------------- roles.toml

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RolesFile {
    pub schema: u32,
    #[serde(default, rename = "role")]
    pub roles: Vec<Role>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoleKind {
    /// Earned from merit; computed, never granted.
    Tier,
    /// Appointed by a grant in `members.toml`.
    Position,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Role {
    pub key: String,
    pub title: String,
    pub kind: RoleKind,
    /// 0..=999 in the repository. 1000 is owners only and comes from engine
    /// configuration.
    pub rank: u32,
    #[serde(default)]
    pub permissions: Vec<String>,
    pub discord_role: Option<String>,
    /// Slack has no roles: a job title is projected as membership in this
    /// Slack user group (an id starting with `S`).
    pub slack_usergroup: Option<String>,
    /// Tier only, required.
    pub min_merit: Option<u64>,
    /// Position only.
    pub max_holders: Option<u32>,
    /// Position only.
    pub term_days: Option<u32>,
    /// Position only: crossing it files a nomination.
    pub merit_eligibility: Option<u64>,
    #[serde(default)]
    pub compartments: Vec<String>,
}

// ------------------------------------------------------------ members.toml

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MembersFile {
    pub schema: u32,
    #[serde(default, rename = "member")]
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemberStatus {
    Active,
    Suspended,
    Removed,
}

impl MemberStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            MemberStatus::Active => "active",
            MemberStatus::Suspended => "suspended",
            MemberStatus::Removed => "removed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Member {
    pub id: String,
    pub name: String,
    pub status: MemberStatus,
    #[serde(default)]
    pub identities: Identities,
    #[serde(default, rename = "grant")]
    pub grants: Vec<Grant>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identities {
    pub github: Option<String>,
    pub gitlab: Option<String>,
    /// Discord user snowflake.
    pub discord: Option<String>,
    /// Slack user id (starts with `U` or `W`).
    pub slack: Option<String>,
}

impl Identities {
    /// `(provider label, value)` for every identity that is set.
    pub fn entries(&self) -> Vec<(&'static str, &str)> {
        let mut v = Vec::new();
        if let Some(x) = &self.github {
            v.push(("github", x.as_str()));
        }
        if let Some(x) = &self.gitlab {
            v.push(("gitlab", x.as_str()));
        }
        if let Some(x) = &self.discord {
            v.push(("discord", x.as_str()));
        }
        if let Some(x) = &self.slack {
            v.push(("slack", x.as_str()));
        }
        v
    }

    /// The login for a provider (GitHub or GitLab), if set.
    pub fn foundry_login(&self, provider: FoundryProvider) -> Option<&str> {
        match provider {
            FoundryProvider::Github => self.github.as_deref(),
            FoundryProvider::Gitlab => self.gitlab.as_deref(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub role: String,
    pub granted_by: String,
    /// Accepts a quoted string `"2027-11-01"` or a TOML date `2027-11-01`.
    #[serde(default, deserialize_with = "de_opt_date")]
    pub expires: Option<NaiveDate>,
}

fn de_opt_date<'de, D: Deserializer<'de>>(d: D) -> Result<Option<NaiveDate>, D::Error> {
    let v: Option<toml::Value> = Option::deserialize(d)?;
    match v {
        None => Ok(None),
        Some(toml::Value::String(s)) => NaiveDate::parse_from_str(&s, "%Y-%m-%d")
            .map(Some)
            .map_err(|e| serde::de::Error::custom(format!("invalid date `{s}`: {e}"))),
        Some(toml::Value::Datetime(dt)) => {
            let s = dt.to_string();
            NaiveDate::parse_from_str(&s, "%Y-%m-%d")
                .map(Some)
                .map_err(|_| serde::de::Error::custom(format!("expected a plain date YYYY-MM-DD, found `{s}`")))
        }
        Some(other) => Err(serde::de::Error::custom(format!(
            "expected a date YYYY-MM-DD, found {}",
            other.type_str()
        ))),
    }
}

// -------------------------------------------------------------- merit.toml

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeritFile {
    pub schema: u32,
    pub rubric: String,
    #[serde(default)]
    pub scorers: Vec<Scorer>,
    pub min_scorers: u32,
    pub score_window_days: u32,
    #[serde(default, rename = "event")]
    pub events: Vec<MeritEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scorer {
    Agent,
    Coderabbit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    PrMerged,
    ReviewApproved,
    IssueClosed,
    ReleasePublished,
    DocsMerged,
    ManualAward,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeritEvent {
    pub key: EventKind,
    pub base_points: u32,
    pub max_points: u32,
    pub weight: f64,
    pub daily_cap: u32,
    pub vesting_days: u32,
    pub external_counts: bool,
}

// ------------------------------------------------------------ payouts.toml

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayoutsFile {
    pub schema: u32,
    /// The kill switch: false means nothing is prepared or paid.
    pub enabled: bool,
    pub currency: String,
    pub schedule: Schedule,
    pub limits: Limits,
    pub approval: Approval,
    pub eligibility: Eligibility,
    #[serde(default, rename = "rate")]
    pub rates: Vec<Rate>,
    /// Closed time entries count as approved without a `time.approve` approval (contract
    /// 14.5). Optional; default false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub time_auto_approve: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cadence {
    Manual,
    Weekly,
    Biweekly,
    Monthly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunMode {
    /// Build a run and wait for approvals and an execute call.
    Prepare,
    /// Pay rows that are already fully approved.
    Auto,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PayoutDay {
    DayOfMonth(u32),
    Weekday(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    pub cadence: Cadence,
    pub day: Option<PayoutDay>,
    pub hour_utc: u32,
    /// The TOML key is `mode`; the field is named `run_mode` so it can never
    /// be confused with the organization mode.
    #[serde(rename = "mode")]
    pub run_mode: RunMode,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub min_payout_cents: u64,
    pub max_per_payout_cents: u64,
    pub max_per_member_per_period_cents: u64,
    pub max_per_run_cents: u64,
    pub hold_days: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    pub approvals_required: u32,
    pub min_approver_rank: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Eligibility {
    pub min_rank: u32,
    #[serde(default)]
    pub roles: Vec<String>,
    pub require_verified_identity: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rate {
    pub branch: String,
    pub base_cents: u64,
    pub merit_multiplier: f64,
}
