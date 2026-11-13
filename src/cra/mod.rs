//! Central Rank Authority (CRA): parse, validate, plan and authorize.
//!
//! The CRA is a set of TOML files in `.dsync/` of an organization's
//! repository (contract `docs/CRA_PLATFORM.md` sections 3 and 4). This module
//! is pure: no database, no network, no clock, no environment. Time is always
//! passed in.
//!
//! Flow for a push to the CRA repository:
//!
//! 1. [`load`] the five files at the pushed commit into a [`Snapshot`]
//!    (parse plus [`Snapshot::validate`]). Any error rejects the snapshot.
//! 2. [`plan_change`] diffs the active snapshot against the new one.
//! 3. [`authorize`] checks the plan against the ACTIVE snapshot.
//! 4. [`standing`] and [`discord_projection`] compute what members hold.
//!
//! The engine side (database, network, clock) lives in [`store`], [`source`],
//! [`loader`] and [`api`]; the pure modules above never call it.

pub mod api;
pub mod authz;
pub mod catalog;
pub mod loader;
pub mod model;
pub mod plan;
pub mod policy;
pub mod source;
pub mod standing;
pub mod store;
pub mod validate;

#[cfg(test)]
mod tests;

use std::fmt;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use authz::{authorize, ChangeContext, Violation, ViolationCode};
pub use catalog::PermissionClass;
pub use model::*;
pub use plan::{plan_change, ChangePlan};
pub use policy::{effective_policy, mode_defaults, Mode, Policy};
pub use standing::{discord_projection, standing, DiscordProjection, Standing};
pub use loader::{load_commit, ActiveCra, CraContext, CraHandle, CraSettings, LoadOutcome};
pub use source::{CraRepo, CraSource, GitHubSource, GitLabSource, SourceError};

/// The schema version this engine reads.
pub const SCHEMA_VERSION: i64 = 1;

/// The rubric ids the engine knows.
pub const KNOWN_RUBRICS: &[&str] = &["dsync-rubric-v1"];

/// The raw contents of the five CRA files, as fetched at one commit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CraFiles {
    pub org: String,
    pub roles: String,
    pub members: String,
    pub merit: String,
    pub payouts: String,
}

impl CraFiles {
    /// The files in the fixed hashing order, with their names.
    pub fn ordered(&self) -> [(CraFileKind, &str); 5] {
        [
            (CraFileKind::Org, self.org.as_str()),
            (CraFileKind::Roles, self.roles.as_str()),
            (CraFileKind::Members, self.members.as_str()),
            (CraFileKind::Merit, self.merit.as_str()),
            (CraFileKind::Payouts, self.payouts.as_str()),
        ]
    }

    /// SHA-256 over the five files in the fixed order org, roles, members,
    /// merit, payouts. Each file is framed as
    /// `<file name> \n <byte length> \n <bytes> \n`, so moving bytes from one
    /// file to the next always changes the hash. Lowercase hex.
    pub fn content_hash(&self) -> String {
        let mut h = Sha256::new();
        for (kind, content) in self.ordered() {
            h.update(kind.file_name().as_bytes());
            h.update(b"\n");
            h.update(content.len().to_string().as_bytes());
            h.update(b"\n");
            h.update(content.as_bytes());
            h.update(b"\n");
        }
        hex::encode(h.finalize())
    }
}

/// Which CRA file an error belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CraFileKind {
    Org,
    Roles,
    Members,
    Merit,
    Payouts,
}

impl CraFileKind {
    pub fn file_name(self) -> &'static str {
        match self {
            CraFileKind::Org => "org.toml",
            CraFileKind::Roles => "roles.toml",
            CraFileKind::Members => "members.toml",
            CraFileKind::Merit => "merit.toml",
            CraFileKind::Payouts => "payouts.toml",
        }
    }
}

/// Every reason a CRA can be refused. Tests match on these codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ErrorCode {
    /// TOML syntax, a wrong type, or a missing required key.
    Parse,
    /// A key the schema does not define.
    UnknownKey,
    /// `schema` missing or not 1.
    SchemaVersion,
    /// A slug, key or id outside `[a-z0-9-]`, or empty.
    InvalidKey,
    /// A required text value is empty.
    EmptyValue,
    /// A bad `[policy]` override.
    PolicyOverride,
    /// A Discord id that is not a snowflake (17 to 20 digits).
    InvalidSnowflake,
    DuplicateForge,
    InvalidRepo,
    /// A non-canonical (mirror) repository lists payout branches.
    MirrorPaysOut,
    DuplicateConsumer,
    UnknownScope,
    UnknownConsumerField,
    DuplicatePermission,
    InvalidPermission,
    DuplicateCompartment,
    DuplicateRoleKey,
    /// Rank 1000 or above in the repository.
    RankOutOfRange,
    TierMissingMinMerit,
    /// `min_merit` on a position.
    TierOnlyField,
    /// `max_holders`, `term_days` or `merit_eligibility` on a tier.
    PositionOnlyField,
    /// A tier holds an authority-class permission and the policy does not
    /// allow merit to confer authority.
    TierAuthorityPermission,
    /// A tier's rank exceeds `promotion.merit_rank_ceiling`.
    TierAboveCeiling,
    DuplicateDiscordRole,
    UnknownPermission,
    UnknownCompartment,
    CompartmentsDisabled,
    /// A position at or above `grants.two_person_min_rank` with `term_days`
    /// longer than `grants.max_term_days`.
    TermExceedsPolicy,
    InvalidValue,
    DuplicateMemberId,
    DuplicateIdentity,
    UnknownRole,
    /// A grant names a tier. Tiers are computed, never granted.
    GrantToTier,
    UnknownGranter,
    /// `granted_by` is the member themselves.
    SelfGrantedBy,
    DuplicateGrant,
    GrantMissingExpiry,
    GrantTermTooLong,
    MaxHoldersExceeded,
    UnknownRubric,
    DuplicateScorer,
    MinScorers,
    DuplicateEvent,
    /// An event lets non-members earn while the policy says they cannot.
    ExternalMeritDisallowed,
    /// `approvals_required < 1`.
    ApprovalsRequiredTooLow,
    InvalidSchedule,
    InvalidLimits,
    /// `limits.hold_days` below the policy's `payouts.hold_days`.
    HoldBelowPolicy,
    DuplicateRate,
    /// A rate names a branch no canonical forge pays out on.
    RateBranchNotPayable,
    /// `eligibility.roles` names a tier while the policy pays positions only.
    EligibilityConflictsPolicy,
}

/// One validation or parse error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CraError {
    pub file: CraFileKind,
    pub code: ErrorCode,
    pub message: String,
}

impl fmt::Display for CraError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {:?}: {}", self.file.file_name(), self.code, self.message)
    }
}

impl CraError {
    pub(crate) fn new(file: CraFileKind, code: ErrorCode, message: impl Into<String>) -> Self {
        CraError { file, code, message: message.into() }
    }
}

/// A forge-authenticated identity: who pushed, merged, authored or approved.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ForgeIdentity {
    pub provider: ForgeProvider,
    pub login: String,
}

impl ForgeIdentity {
    pub fn github(login: &str) -> Self {
        ForgeIdentity { provider: ForgeProvider::Github, login: login.to_string() }
    }
    pub fn gitlab(login: &str) -> Self {
        ForgeIdentity { provider: ForgeProvider::Gitlab, login: login.to_string() }
    }
    /// GitHub and GitLab logins are case-insensitive.
    pub fn matches(&self, provider: ForgeProvider, login: &str) -> bool {
        self.provider == provider && self.login.eq_ignore_ascii_case(login)
    }
}

impl fmt::Display for ForgeIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.provider.as_str(), self.login)
    }
}

/// A parsed CRA at one commit.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// [`CraFiles::content_hash`] of the files this was parsed from.
    pub content_hash: String,
    pub org: OrgFile,
    pub roles: RolesFile,
    pub members: MembersFile,
    pub merit: MeritFile,
    pub payouts: PayoutsFile,
    /// The organization mode as a label, for display and change detection
    /// only. Decisions read [`Snapshot::policy`].
    pub mode_label: Mode,
    /// The effective policy (mode defaults plus overrides).
    pub policy: Policy,
}

fn parse_file<T: serde::de::DeserializeOwned>(
    kind: CraFileKind,
    text: &str,
    errors: &mut Vec<CraError>,
) -> Option<T> {
    let value: toml::Value = match toml::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            errors.push(CraError::new(kind, ErrorCode::Parse, e.to_string()));
            return None;
        }
    };
    match value.get("schema") {
        Some(toml::Value::Integer(SCHEMA_VERSION)) => {}
        Some(other) => {
            errors.push(CraError::new(
                kind,
                ErrorCode::SchemaVersion,
                format!("schema must be {SCHEMA_VERSION}, found {other}"),
            ));
            return None;
        }
        None => {
            errors.push(CraError::new(
                kind,
                ErrorCode::SchemaVersion,
                format!("the file must start with `schema = {SCHEMA_VERSION}`"),
            ));
            return None;
        }
    }
    match toml::from_str::<T>(text) {
        Ok(v) => Some(v),
        Err(e) => {
            let msg = e.to_string();
            let code = if msg.contains("unknown field") {
                ErrorCode::UnknownKey
            } else {
                ErrorCode::Parse
            };
            errors.push(CraError::new(kind, code, msg));
            None
        }
    }
}

impl Snapshot {
    /// Parses the five files and resolves the effective policy. This checks
    /// syntax, `schema = 1`, unknown keys, types and policy overrides, and
    /// returns every such error. It does NOT run the cross-file rules; use
    /// [`load`] (parse plus [`Snapshot::validate`]) for anything that will
    /// become active. A parsed but unvalidated snapshot is useful only for
    /// the change preview.
    pub fn parse(files: &CraFiles) -> Result<Snapshot, Vec<CraError>> {
        let mut errors = Vec::new();
        let org: Option<OrgFile> = parse_file(CraFileKind::Org, &files.org, &mut errors);
        let roles: Option<RolesFile> = parse_file(CraFileKind::Roles, &files.roles, &mut errors);
        let members: Option<MembersFile> = parse_file(CraFileKind::Members, &files.members, &mut errors);
        let merit: Option<MeritFile> = parse_file(CraFileKind::Merit, &files.merit, &mut errors);
        let payouts: Option<PayoutsFile> = parse_file(CraFileKind::Payouts, &files.payouts, &mut errors);

        let mut resolved = None;
        if let Some(org) = &org {
            let (label, policy) = policy::resolve_org_policy(org);
            match policy {
                Ok(p) => resolved = Some((label, p)),
                Err(msgs) => errors.extend(
                    msgs.into_iter()
                        .map(|m| CraError::new(CraFileKind::Org, ErrorCode::PolicyOverride, m)),
                ),
            }
        }

        match (org, roles, members, merit, payouts, resolved) {
            (Some(org), Some(roles), Some(members), Some(merit), Some(payouts), Some((mode_label, policy)))
                if errors.is_empty() =>
            {
                Ok(Snapshot {
                    content_hash: files.content_hash(),
                    org,
                    roles,
                    members,
                    merit,
                    payouts,
                    mode_label,
                    policy,
                })
            }
            _ => Err(errors),
        }
    }

    /// Runs every rule of contract section 3 plus the extra rules listed in
    /// `docs/CRA_CONFIG.md`, as of `today` (used only for grant expiry
    /// limits). Returns every error, not just the first.
    pub fn validate(&self, today: NaiveDate) -> Result<(), Vec<CraError>> {
        let errors = validate::validate(self, today);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    pub fn role(&self, key: &str) -> Option<&Role> {
        self.roles.roles.iter().find(|r| r.key == key)
    }

    pub fn member(&self, id: &str) -> Option<&Member> {
        self.members.members.iter().find(|m| m.id == id)
    }

    /// The member whose identities list this forge identity.
    pub fn member_by_forge(&self, who: &ForgeIdentity) -> Option<&Member> {
        self.members.members.iter().find(|m| {
            m.identities
                .forge_login(who.provider)
                .map(|l| who.matches(who.provider, l))
                .unwrap_or(false)
        })
    }

    /// The permission class of `key` from the catalog or `permissions_extra`.
    pub fn permission_class(&self, key: &str) -> Option<PermissionClass> {
        catalog::catalog_class(key).or_else(|| {
            self.org
                .permissions_extra
                .iter()
                .find(|p| p.key == key)
                .map(|p| p.class)
        })
    }
}

/// Parses and validates. This is the only way a snapshot should become
/// active.
pub fn load(files: &CraFiles, today: NaiveDate) -> Result<Snapshot, Vec<CraError>> {
    let snap = Snapshot::parse(files)?;
    snap.validate(today)?;
    Ok(snap)
}
