//! The organization lever (hierarchy doc 3).
//!
//! `effective_policy = mode_defaults(mode) <- overrides`, field by field.
//! The organization's mode is read in exactly one function,
//! [`resolve_org_policy`]. Everything else reads [`Policy`] fields. A test in
//! `cra::tests` scans `src/cra/` and fails if the mode field is read anywhere
//! else.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::model::OrgFile;

/// The organization mode. It is a label that selects defaults, never a flag
/// that code branches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Open,
    /// Public code under a license that is not an open-source license
    /// (contract 3.1.1). Written `source_available` in `org.toml`.
    SourceAvailable,
    Hybrid,
    Closed,
}

impl Mode {
    /// Every mode, in contract table order.
    pub const ALL: [Mode; 4] = [Mode::Open, Mode::SourceAvailable, Mode::Hybrid, Mode::Closed];

    /// The value written in `org.toml`.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Open => "open",
            Mode::SourceAvailable => "source_available",
            Mode::Hybrid => "hybrid",
            Mode::Closed => "closed",
        }
    }

    /// The human label.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Open => "Open",
            Mode::SourceAvailable => "Source available",
            Mode::Hybrid => "Hybrid",
            Mode::Closed => "Closed",
        }
    }
}

/// Visibility values used by the `visibility.*` fields. Each field accepts
/// only a subset (see [`Policy`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Public,
    Members,
    NeedToKnow,
    SelfAndChain,
    Off,
}

impl Visibility {
    fn as_str(self) -> &'static str {
        match self {
            Visibility::Public => "public",
            Visibility::Members => "members",
            Visibility::NeedToKnow => "need_to_know",
            Visibility::SelfAndChain => "self_and_chain",
            Visibility::Off => "off",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "public" => Visibility::Public,
            "members" => Visibility::Members,
            "need_to_know" => Visibility::NeedToKnow,
            "self_and_chain" => Visibility::SelfAndChain,
            "off" => Visibility::Off,
            _ => return None,
        })
    }
}

/// `merit.external_contributions_earn`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalEarn {
    Yes,
    PublicReposOnly,
    No,
}

/// `payouts.eligible`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PayoutEligible {
    VerifiedMembers,
    PositionsOnly,
}

/// `audit.visibility`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditVisibility {
    MembersSummary,
    Officers,
}

/// `anomaly.action_on_trip`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnomalyAction {
    Alert,
    AlertAndFreezeReads,
}

/// The effective policy. Field names follow hierarchy doc 3.2; the dotted
/// override key of each field is listed in [`POLICY_KEYS`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    pub visibility_roster: Visibility,
    pub visibility_leaderboard: Visibility,
    pub visibility_profile: Visibility,
    pub visibility_positions: Visibility,
    pub merit_external_contributions_earn: ExternalEarn,
    /// Source patterns. `["*"]` means all sources.
    pub merit_sources_enabled: Vec<String>,
    pub promotion_merit_rank_ceiling: u32,
    pub promotion_merit_may_confer_authority: bool,
    pub promotion_eligibility_nominations: bool,
    pub compartments_enabled: bool,
    pub probation_days: u32,
    pub payouts_eligible: PayoutEligible,
    pub payouts_hold_days: u32,
    pub payouts_two_person_min_cents: u64,
    pub grants_two_person_min_rank: u32,
    pub grants_max_term_days: u32,
    pub audit_visibility: AuditVisibility,
    pub anomaly_profile_reads_per_hour: u32,
    pub anomaly_action_on_trip: AnomalyAction,
    /// Always `pairwise`; no other value is accepted.
    pub consumers_subject_ids: String,
}

/// Every override key accepted in `org.toml` `[policy]`, in table order.
pub const POLICY_KEYS: &[&str] = &[
    "visibility.roster",
    "visibility.leaderboard",
    "visibility.profile",
    "visibility.positions",
    "merit.external_contributions_earn",
    "merit.sources_enabled",
    "promotion.merit_rank_ceiling",
    "promotion.merit_may_confer_authority",
    "promotion.eligibility_nominations",
    "compartments.enabled",
    "probation.days",
    "payouts.eligible",
    "payouts.hold_days",
    "payouts.two_person_min_cents",
    "grants.two_person_min_rank",
    "grants.max_term_days",
    "audit.visibility",
    "anomaly.profile_reads_per_hour",
    "anomaly.action_on_trip",
    "consumers.subject_ids",
];

/// The per-mode defaults of hierarchy doc 3.2 and contract 3.1.1.
pub fn mode_defaults(mode: Mode) -> Policy {
    use Visibility::*;
    match mode {
        Mode::Open => Policy {
            visibility_roster: Public,
            visibility_leaderboard: Public,
            visibility_profile: Public,
            visibility_positions: Public,
            merit_external_contributions_earn: ExternalEarn::Yes,
            merit_sources_enabled: vec!["*".into()],
            promotion_merit_rank_ceiling: 30,
            promotion_merit_may_confer_authority: false,
            promotion_eligibility_nominations: true,
            compartments_enabled: false,
            probation_days: 14,
            payouts_eligible: PayoutEligible::VerifiedMembers,
            payouts_hold_days: 7,
            payouts_two_person_min_cents: 50_000,
            grants_two_person_min_rank: 60,
            grants_max_term_days: 365,
            audit_visibility: AuditVisibility::MembersSummary,
            anomaly_profile_reads_per_hour: 300,
            anomaly_action_on_trip: AnomalyAction::Alert,
            consumers_subject_ids: "pairwise".into(),
        },
        // Contract 3.1.1: as hybrid, except that outside contributions earn
        // in every listed repository, not only public ones.
        Mode::SourceAvailable => Policy {
            visibility_roster: Members,
            visibility_leaderboard: Members,
            visibility_profile: Members,
            visibility_positions: Members,
            merit_external_contributions_earn: ExternalEarn::Yes,
            merit_sources_enabled: vec!["*".into()],
            promotion_merit_rank_ceiling: 30,
            promotion_merit_may_confer_authority: false,
            promotion_eligibility_nominations: true,
            compartments_enabled: true,
            probation_days: 30,
            payouts_eligible: PayoutEligible::VerifiedMembers,
            payouts_hold_days: 7,
            payouts_two_person_min_cents: 25_000,
            grants_two_person_min_rank: 50,
            grants_max_term_days: 180,
            audit_visibility: AuditVisibility::Officers,
            anomaly_profile_reads_per_hour: 120,
            anomaly_action_on_trip: AnomalyAction::Alert,
            consumers_subject_ids: "pairwise".into(),
        },
        Mode::Hybrid => Policy {
            visibility_roster: Members,
            visibility_leaderboard: Members,
            visibility_profile: Members,
            visibility_positions: Members,
            merit_external_contributions_earn: ExternalEarn::PublicReposOnly,
            merit_sources_enabled: vec!["*".into()],
            promotion_merit_rank_ceiling: 30,
            promotion_merit_may_confer_authority: false,
            promotion_eligibility_nominations: true,
            compartments_enabled: true,
            probation_days: 30,
            payouts_eligible: PayoutEligible::VerifiedMembers,
            payouts_hold_days: 7,
            payouts_two_person_min_cents: 25_000,
            grants_two_person_min_rank: 50,
            grants_max_term_days: 180,
            audit_visibility: AuditVisibility::Officers,
            anomaly_profile_reads_per_hour: 120,
            anomaly_action_on_trip: AnomalyAction::Alert,
            consumers_subject_ids: "pairwise".into(),
        },
        Mode::Closed => Policy {
            visibility_roster: NeedToKnow,
            visibility_leaderboard: Off,
            visibility_profile: SelfAndChain,
            visibility_positions: NeedToKnow,
            merit_external_contributions_earn: ExternalEarn::No,
            merit_sources_enabled: vec!["internal.*".into(), "manual.award".into()],
            promotion_merit_rank_ceiling: 0,
            promotion_merit_may_confer_authority: false,
            promotion_eligibility_nominations: false,
            compartments_enabled: true,
            probation_days: 90,
            payouts_eligible: PayoutEligible::PositionsOnly,
            payouts_hold_days: 14,
            payouts_two_person_min_cents: 0,
            grants_two_person_min_rank: 30,
            grants_max_term_days: 90,
            audit_visibility: AuditVisibility::Officers,
            anomaly_profile_reads_per_hour: 60,
            anomaly_action_on_trip: AnomalyAction::AlertAndFreezeReads,
            consumers_subject_ids: "pairwise".into(),
        },
    }
}

/// Flattens `[policy]` so that both `"payouts.hold_days" = 10` and the TOML
/// dotted form `payouts.hold_days = 10` (a nested table) produce the key
/// `payouts.hold_days`.
pub fn flatten_overrides(raw: &BTreeMap<String, toml::Value>) -> BTreeMap<String, toml::Value> {
    fn walk(prefix: &str, v: &toml::Value, out: &mut BTreeMap<String, toml::Value>) {
        match v {
            toml::Value::Table(t) => {
                for (k, v) in t {
                    walk(&format!("{prefix}.{k}"), v, out);
                }
            }
            other => {
                out.insert(prefix.to_string(), other.clone());
            }
        }
    }
    let mut out = BTreeMap::new();
    for (k, v) in raw {
        walk(k, v, &mut out);
    }
    out
}

/// `mode_defaults(mode)` with `overrides` applied field by field. Unknown
/// keys, wrong types and values outside a field's allowed set are errors;
/// every error is returned.
pub fn effective_policy(
    mode: Mode,
    overrides: &BTreeMap<String, toml::Value>,
) -> Result<Policy, Vec<String>> {
    let mut p = mode_defaults(mode);
    let mut errors = Vec::new();
    for (key, value) in flatten_overrides(overrides) {
        if let Err(e) = apply_override(&mut p, &key, &value) {
            errors.push(format!("policy override `{key}`: {e}"));
        }
    }
    if errors.is_empty() {
        Ok(p)
    } else {
        Err(errors)
    }
}

/// The ONLY function that reads the organization's mode field. It returns
/// the mode as a label (for display and change detection) plus the effective
/// policy that every decision reads.
pub fn resolve_org_policy(org: &OrgFile) -> (Mode, Result<Policy, Vec<String>>) {
    let mode = org.org.mode;
    (mode, effective_policy(mode, &org.policy))
}

fn want_str(v: &toml::Value) -> Result<&str, String> {
    v.as_str().ok_or_else(|| format!("expected a string, found {}", v.type_str()))
}
fn want_bool(v: &toml::Value) -> Result<bool, String> {
    v.as_bool().ok_or_else(|| format!("expected true or false, found {}", v.type_str()))
}
fn want_uint(v: &toml::Value, max: u64) -> Result<u64, String> {
    match v.as_integer() {
        Some(i) if i >= 0 && (i as u64) <= max => Ok(i as u64),
        Some(i) => Err(format!("{i} is out of range 0..={max}")),
        None => Err(format!("expected an integer, found {}", v.type_str())),
    }
}
fn want_vis(v: &toml::Value, allowed: &[Visibility]) -> Result<Visibility, String> {
    let s = want_str(v)?;
    match Visibility::parse(s) {
        Some(x) if allowed.contains(&x) => Ok(x),
        _ => Err(format!(
            "`{s}` is not one of {}",
            allowed.iter().map(|a| a.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

const ROSTER_VIS: &[Visibility] = &[Visibility::Public, Visibility::Members, Visibility::NeedToKnow];
const LEADERBOARD_VIS: &[Visibility] = &[Visibility::Public, Visibility::Members, Visibility::Off];
const PROFILE_VIS: &[Visibility] = &[Visibility::Public, Visibility::Members, Visibility::SelfAndChain];

fn apply_override(p: &mut Policy, key: &str, v: &toml::Value) -> Result<(), String> {
    match key {
        "visibility.roster" => p.visibility_roster = want_vis(v, ROSTER_VIS)?,
        "visibility.leaderboard" => p.visibility_leaderboard = want_vis(v, LEADERBOARD_VIS)?,
        "visibility.profile" => p.visibility_profile = want_vis(v, PROFILE_VIS)?,
        "visibility.positions" => p.visibility_positions = want_vis(v, ROSTER_VIS)?,
        "merit.external_contributions_earn" => {
            p.merit_external_contributions_earn = match v {
                toml::Value::Boolean(true) => ExternalEarn::Yes,
                toml::Value::Boolean(false) => ExternalEarn::No,
                toml::Value::String(s) if s == "public_repos_only" => ExternalEarn::PublicReposOnly,
                other => {
                    return Err(format!(
                        "expected true, false or \"public_repos_only\", found {other}"
                    ))
                }
            }
        }
        "merit.sources_enabled" => {
            p.merit_sources_enabled = match v {
                toml::Value::String(s) if s == "all" => vec!["*".into()],
                toml::Value::Array(items) => {
                    let mut out = Vec::new();
                    for it in items {
                        out.push(want_str(it)?.to_string());
                    }
                    out
                }
                other => return Err(format!("expected \"all\" or a list of strings, found {other}")),
            }
        }
        "promotion.merit_rank_ceiling" => p.promotion_merit_rank_ceiling = want_uint(v, 999)? as u32,
        "promotion.merit_may_confer_authority" => p.promotion_merit_may_confer_authority = want_bool(v)?,
        "promotion.eligibility_nominations" => p.promotion_eligibility_nominations = want_bool(v)?,
        "compartments.enabled" => p.compartments_enabled = want_bool(v)?,
        "probation.days" => p.probation_days = want_uint(v, 3650)? as u32,
        "payouts.eligible" => {
            p.payouts_eligible = match want_str(v)? {
                "verified_members" => PayoutEligible::VerifiedMembers,
                "positions_only" => PayoutEligible::PositionsOnly,
                s => return Err(format!("`{s}` is not one of verified_members, positions_only")),
            }
        }
        "payouts.hold_days" => p.payouts_hold_days = want_uint(v, 3650)? as u32,
        "payouts.two_person_min_cents" => p.payouts_two_person_min_cents = want_uint(v, u64::MAX >> 1)?,
        "grants.two_person_min_rank" => p.grants_two_person_min_rank = want_uint(v, 999)? as u32,
        "grants.max_term_days" => p.grants_max_term_days = want_uint(v, 3650)? as u32,
        "audit.visibility" => {
            p.audit_visibility = match want_str(v)? {
                "members_summary" => AuditVisibility::MembersSummary,
                "officers" => AuditVisibility::Officers,
                s => return Err(format!("`{s}` is not one of members_summary, officers")),
            }
        }
        "anomaly.profile_reads_per_hour" => p.anomaly_profile_reads_per_hour = want_uint(v, 1_000_000)? as u32,
        "anomaly.action_on_trip" => {
            p.anomaly_action_on_trip = match want_str(v)? {
                "alert" => AnomalyAction::Alert,
                "alert_and_freeze_reads" => AnomalyAction::AlertAndFreezeReads,
                s => return Err(format!("`{s}` is not one of alert, alert_and_freeze_reads")),
            }
        }
        "consumers.subject_ids" => {
            let s = want_str(v)?;
            if s != "pairwise" {
                return Err(format!("`{s}` is not allowed; only pairwise"));
            }
            p.consumers_subject_ids = s.to_string();
        }
        _ => return Err("unknown policy key".to_string()),
    }
    Ok(())
}

impl Policy {
    /// Every field as `(override key, rendered value)`, in table order. Used
    /// by the change plan to diff two policies field by field.
    pub fn entries(&self) -> Vec<(&'static str, String)> {
        let ee = match self.merit_external_contributions_earn {
            ExternalEarn::Yes => "true",
            ExternalEarn::PublicReposOnly => "public_repos_only",
            ExternalEarn::No => "false",
        };
        let pe = match self.payouts_eligible {
            PayoutEligible::VerifiedMembers => "verified_members",
            PayoutEligible::PositionsOnly => "positions_only",
        };
        let av = match self.audit_visibility {
            AuditVisibility::MembersSummary => "members_summary",
            AuditVisibility::Officers => "officers",
        };
        let aa = match self.anomaly_action_on_trip {
            AnomalyAction::Alert => "alert",
            AnomalyAction::AlertAndFreezeReads => "alert_and_freeze_reads",
        };
        let values = vec![
            self.visibility_roster.as_str().to_string(),
            self.visibility_leaderboard.as_str().to_string(),
            self.visibility_profile.as_str().to_string(),
            self.visibility_positions.as_str().to_string(),
            ee.to_string(),
            format!("[{}]", self.merit_sources_enabled.join(", ")),
            self.promotion_merit_rank_ceiling.to_string(),
            self.promotion_merit_may_confer_authority.to_string(),
            self.promotion_eligibility_nominations.to_string(),
            self.compartments_enabled.to_string(),
            self.probation_days.to_string(),
            pe.to_string(),
            self.payouts_hold_days.to_string(),
            self.payouts_two_person_min_cents.to_string(),
            self.grants_two_person_min_rank.to_string(),
            self.grants_max_term_days.to_string(),
            av.to_string(),
            self.anomaly_profile_reads_per_hour.to_string(),
            aa.to_string(),
            self.consumers_subject_ids.clone(),
        ];
        POLICY_KEYS.iter().copied().zip(values).collect()
    }
}
