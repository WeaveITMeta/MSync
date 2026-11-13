//! Payout policy (contract section 3.5). The struct mirrors `payouts.toml` exactly so the same
//! file can be loaded from the CRA later; here it is stored per org as versioned JSON.
//!
//! With no stored policy the effective policy reproduces the pre-policy behaviour: one approver,
//! no hold, the env-based rates and cap (`PayoutPolicy::from_config`).

use super::config::{PaymentConfig, DEFAULT_BASE_CENTS};
use crate::auth::{scopes, AuthError, Principal};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

pub const POLICY_SCHEMA: u32 = 1;
const MAX_RANK: i64 = 1000;
const MAX_HOLD_DAYS: i64 = 3650;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayoutPolicy {
    pub schema: u32,
    /// false is the kill switch: nothing is prepared or paid.
    pub enabled: bool,
    pub currency: String,
    pub schedule: Schedule,
    pub limits: Limits,
    pub approval: Approval,
    #[serde(default)]
    pub eligibility: Eligibility,
    #[serde(default)]
    pub rate: Vec<Rate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cadence {
    Manual,
    Weekly,
    Biweekly,
    Monthly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleMode {
    /// Build a run and wait for approvals and an execute call.
    #[default]
    Prepare,
    /// Build a run and pay the rows that are already payable.
    Auto,
}

/// `day = "friday"` (weekly, biweekly) or `day = 15` (monthly, 1..=28).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ScheduleDay {
    MonthDay(i64),
    Weekday(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    pub cadence: Cadence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub day: Option<ScheduleDay>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hour_utc: Option<i64>,
    #[serde(default)]
    pub mode: ScheduleMode,
}

/// All limits are optional; an absent limit does not apply. Values are non-negative.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_payout_cents: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_per_payout_cents: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_per_member_per_period_cents: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_per_run_cents: Option<i64>,
    /// A ledger row cannot be paid before this many days after it was recorded (merge time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_days: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    /// Distinct approvers per ledger row. Minimum 1.
    pub approvals_required: i64,
    #[serde(default)]
    pub min_approver_rank: i64,
    // A payee can never approve their own payout. This is not configurable.
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Eligibility {
    #[serde(default)]
    pub min_rank: i64,
    /// Empty = any role.
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub require_verified_identity: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rate {
    pub branch: String,
    pub base_cents: i64,
    pub merit_multiplier: f64,
}

pub const WEEKDAYS: &[&str] = &["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"];

impl PayoutPolicy {
    /// The effective policy when none is stored: today's behaviour.
    pub fn from_config(cfg: &PaymentConfig) -> Self {
        PayoutPolicy {
            schema: POLICY_SCHEMA,
            enabled: true,
            currency: cfg.currency.clone(),
            schedule: Schedule {
                cadence: Cadence::Manual,
                day: None,
                hour_utc: None,
                mode: ScheduleMode::Prepare,
            },
            limits: Limits {
                min_payout_cents: None,
                max_per_payout_cents: Some(cfg.max_cents),
                max_per_member_per_period_cents: None,
                max_per_run_cents: None,
                hold_days: None,
            },
            approval: Approval {
                approvals_required: 1,
                min_approver_rank: 0,
            },
            eligibility: Eligibility::default(),
            rate: cfg
                .branches
                .iter()
                .map(|b| Rate {
                    branch: b.clone(),
                    base_cents: *cfg.base_cents.get(b).unwrap_or(&DEFAULT_BASE_CENTS),
                    merit_multiplier: cfg.merit_multiplier,
                })
                .collect(),
        }
    }

    /// (base_cents, amount_cents) for a merged change on `branch`, or None if no rate covers it.
    /// amount = base * (1 + merit_points * multiplier), rounded, clamped to [0, max_per_payout_cents].
    pub fn amount_for(&self, branch: &str, merit_points: i64) -> Option<(i64, i64)> {
        let rate = self.rate.iter().find(|r| r.branch == branch)?;
        let factor = 1.0 + (merit_points.max(0) as f64) * rate.merit_multiplier;
        let amount = ((rate.base_cents as f64) * factor).round() as i64;
        let cap = self.limits.max_per_payout_cents.unwrap_or(i64::MAX).max(0);
        Some((rate.base_cents, amount.clamp(0, cap)))
    }

    pub fn hold_days(&self) -> i64 {
        self.limits.hold_days.unwrap_or(0)
    }

    /// Every rule violation; empty means valid.
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if self.schema != POLICY_SCHEMA {
            errs.push(format!("schema must be {POLICY_SCHEMA}"));
        }
        let c = &self.currency;
        if c.len() != 3 || !c.bytes().all(|b| b.is_ascii_lowercase()) {
            errs.push("currency must be a 3-letter lowercase ISO code".into());
        }

        // schedule
        let s = &self.schedule;
        let hour_ok = matches!(s.hour_utc, Some(h) if (0..=23).contains(&h));
        match s.cadence {
            Cadence::Manual => {
                if let Some(h) = s.hour_utc {
                    if !(0..=23).contains(&h) {
                        errs.push("schedule.hour_utc must be 0..=23".into());
                    }
                }
            }
            Cadence::Weekly | Cadence::Biweekly => {
                match &s.day {
                    Some(ScheduleDay::Weekday(d)) if WEEKDAYS.contains(&d.as_str()) => {}
                    _ => errs.push("schedule.day must be a lowercase weekday name for weekly/biweekly".into()),
                }
                if !hour_ok {
                    errs.push("schedule.hour_utc must be 0..=23".into());
                }
            }
            Cadence::Monthly => {
                match &s.day {
                    Some(ScheduleDay::MonthDay(d)) if (1..=28).contains(d) => {}
                    _ => errs.push("schedule.day must be 1..=28 for monthly".into()),
                }
                if !hour_ok {
                    errs.push("schedule.hour_utc must be 0..=23".into());
                }
            }
        }

        // limits
        let l = &self.limits;
        for (name, v) in [
            ("min_payout_cents", l.min_payout_cents),
            ("max_per_payout_cents", l.max_per_payout_cents),
            ("max_per_member_per_period_cents", l.max_per_member_per_period_cents),
            ("max_per_run_cents", l.max_per_run_cents),
            ("hold_days", l.hold_days),
        ] {
            if matches!(v, Some(x) if x < 0) {
                errs.push(format!("limits.{name} must be non-negative"));
            }
        }
        if matches!(l.hold_days, Some(h) if h > MAX_HOLD_DAYS) {
            errs.push(format!("limits.hold_days must be <= {MAX_HOLD_DAYS}"));
        }
        if let (Some(min), Some(max)) = (l.min_payout_cents, l.max_per_payout_cents) {
            if min > max {
                errs.push("limits.min_payout_cents must be <= max_per_payout_cents".into());
            }
        }

        // approval
        if self.approval.approvals_required < 1 {
            errs.push("approval.approvals_required must be >= 1".into());
        }
        if !(0..=MAX_RANK).contains(&self.approval.min_approver_rank) {
            errs.push("approval.min_approver_rank must be 0..=1000".into());
        }

        // eligibility
        if !(0..=MAX_RANK).contains(&self.eligibility.min_rank) {
            errs.push("eligibility.min_rank must be 0..=1000".into());
        }
        if self.eligibility.roles.iter().any(|r| r.trim().is_empty()) {
            errs.push("eligibility.roles must not contain empty keys".into());
        }

        // rates
        let mut seen: Vec<&str> = Vec::new();
        for r in &self.rate {
            if r.branch.trim().is_empty() {
                errs.push("rate.branch must not be empty".into());
            } else if seen.contains(&r.branch.as_str()) {
                errs.push(format!("rate.branch '{}' is listed twice", r.branch));
            } else {
                seen.push(&r.branch);
            }
            if r.base_cents < 0 {
                errs.push(format!("rate '{}': base_cents must be non-negative", r.branch));
            }
            if !r.merit_multiplier.is_finite() || r.merit_multiplier < 0.0 {
                errs.push(format!("rate '{}': merit_multiplier must be a non-negative number", r.branch));
            }
        }
        errs
    }
}

/// The policy in force for an org: the latest stored version, or the env default.
#[derive(Debug, Clone, Serialize)]
pub struct EffectivePolicy {
    pub policy: PayoutPolicy,
    /// None when no policy is stored (the env default is in force).
    pub version: Option<i64>,
    pub updated_by: Option<String>,
    pub updated_at: Option<String>,
}

impl EffectivePolicy {
    pub fn is_stored(&self) -> bool {
        self.version.is_some()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("{0}")]
    Auth(#[from] AuthError),
    #[error("invalid policy: {}", .0.join("; "))]
    Invalid(Vec<String>),
    #[error("policy version conflict: current version is {0:?}")]
    Conflict(Option<i64>),
    /// A stored policy that no longer parses or validates. Fail closed: never fall back to the default.
    #[error("stored payout policy is unusable: {0}")]
    Corrupt(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Load the effective policy. A stored policy that fails to parse or validate is an error.
///
/// While a CRA snapshot is active for `org`, the policy is its `payouts.toml` (the stored
/// versions are not consulted, and a snapshot whose `payouts.toml` does not convert is
/// `Corrupt`). With no active snapshot this is exactly the pre-CRA behaviour.
pub async fn load_effective(pool: &SqlitePool, org: &str, cfg: &PaymentConfig) -> Result<EffectivePolicy, PolicyError> {
    if let Some(eff) = crate::cra::loader::cra_payout_policy(pool, org).await? {
        return Ok(eff);
    }
    let row: Option<(i64, String, String, String)> = sqlx::query_as(
        "SELECT version, policy_json, updated_by, created_at FROM payout_policies WHERE org_id = ? ORDER BY version DESC LIMIT 1",
    )
    .bind(org)
    .fetch_optional(pool)
    .await?;
    match row {
        None => Ok(EffectivePolicy {
            policy: PayoutPolicy::from_config(cfg),
            version: None,
            updated_by: None,
            updated_at: None,
        }),
        Some((version, json, by, at)) => {
            let policy: PayoutPolicy =
                serde_json::from_str(&json).map_err(|e| PolicyError::Corrupt(format!("version {version}: {e}")))?;
            let errs = policy.validate();
            if !errs.is_empty() {
                return Err(PolicyError::Corrupt(format!("version {version}: {}", errs.join("; "))));
            }
            Ok(EffectivePolicy {
                policy,
                version: Some(version),
                updated_by: Some(by),
                updated_at: Some(at),
            })
        }
    }
}

/// GET policy (`payouts.read`).
pub async fn get_policy(pool: &SqlitePool, cfg: &PaymentConfig, principal: &Principal) -> Result<EffectivePolicy, PolicyError> {
    principal.require(scopes::PAYOUTS_READ)?;
    load_effective(pool, &principal.org_id, cfg).await
}

/// PUT policy (`payouts.policy`). Validates, then stores a new version. With `expected_version`
/// the write is refused unless the current version matches (None = "no stored policy yet").
pub async fn put_policy(
    pool: &SqlitePool,
    principal: &Principal,
    policy: &PayoutPolicy,
    expected_version: Option<Option<i64>>,
) -> Result<i64, PolicyError> {
    principal.require(scopes::PAYOUTS_POLICY)?;
    // The CRA authors the policy while a snapshot is active. The HTTP route answers 409
    // before reaching here (`cra::api`); this refuses any other caller.
    if crate::cra::loader::cra_active(pool, &principal.org_id).await? {
        return Err(PolicyError::Invalid(vec![crate::cra::loader::POLICY_AUTHORED_IN_CRA.to_string()]));
    }
    let errs = policy.validate();
    if !errs.is_empty() {
        return Err(PolicyError::Invalid(errs));
    }
    let json = serde_json::to_string(policy).map_err(|e| PolicyError::Invalid(vec![e.to_string()]))?;

    let mut tx = pool.begin().await?;
    let current: Option<i64> = sqlx::query_scalar("SELECT MAX(version) FROM payout_policies WHERE org_id = ?")
        .bind(&principal.org_id)
        .fetch_one(&mut *tx)
        .await?;
    if let Some(expected) = expected_version {
        if expected != current {
            return Err(PolicyError::Conflict(current));
        }
    }
    let next = current.unwrap_or(0) + 1;
    let res = sqlx::query(
        "INSERT INTO payout_policies (org_id, version, policy_json, updated_by) VALUES (?, ?, ?, ?)",
    )
    .bind(&principal.org_id)
    .bind(next)
    .bind(&json)
    .bind(&principal.member_id)
    .execute(&mut *tx)
    .await;
    match res {
        Ok(_) => {}
        Err(sqlx::Error::Database(e)) if e.message().contains("UNIQUE") => return Err(PolicyError::Conflict(current)),
        Err(e) => return Err(e.into()),
    }
    tx.commit().await?;
    Ok(next)
}
