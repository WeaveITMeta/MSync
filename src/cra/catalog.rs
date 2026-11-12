//! The permission catalog (hierarchy doc 4.4).
//!
//! Every permission is a `surface.action` string with a class. The class
//! decides who may hold it: an earned tier may never hold an
//! `authority` permission unless the effective policy sets
//! `promotion.merit_may_confer_authority`.

use serde::{Deserialize, Serialize};

/// The class of a permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionClass {
    /// Participation.
    Basic,
    /// Reads that enable exfiltration.
    Sensitive,
    /// Changes who holds power or money.
    Authority,
}

/// The built-in catalog. Order follows the hierarchy document table.
pub const CATALOG: &[(&str, PermissionClass)] = &[
    ("profile.self", PermissionClass::Basic),
    ("community.read", PermissionClass::Basic),
    ("community.post", PermissionClass::Basic),
    ("community.vote", PermissionClass::Basic),
    ("community.events", PermissionClass::Basic),
    ("community.moderate", PermissionClass::Basic),
    ("leaderboard.read", PermissionClass::Basic),
    ("roster.read_public", PermissionClass::Basic),
    ("roster.read_members", PermissionClass::Basic),
    ("roster.read_full", PermissionClass::Sensitive),
    ("merit.read_own", PermissionClass::Basic),
    ("merit.read_all", PermissionClass::Sensitive),
    ("merit.award_manual", PermissionClass::Authority),
    ("merit.clawback", PermissionClass::Authority),
    ("members.invite", PermissionClass::Authority),
    ("members.grant", PermissionClass::Authority),
    ("members.approve_grant", PermissionClass::Authority),
    ("members.revoke", PermissionClass::Authority),
    ("members.suspend", PermissionClass::Authority),
    ("members.ban", PermissionClass::Authority),
    ("payouts.read_own", PermissionClass::Basic),
    ("payouts.read_all", PermissionClass::Sensitive),
    ("payouts.approve", PermissionClass::Authority),
    ("policy.read", PermissionClass::Basic),
    ("policy.propose_change", PermissionClass::Authority),
    ("policy.approve_change", PermissionClass::Authority),
    ("audit.read_own", PermissionClass::Basic),
    ("audit.read_all", PermissionClass::Sensitive),
    ("audit.verify", PermissionClass::Basic),
    ("compartment.manage", PermissionClass::Authority),
    ("export.roster", PermissionClass::Sensitive),
    ("export.audit", PermissionClass::Sensitive),
    ("discord.reconcile", PermissionClass::Authority),
    ("consumers.manage", PermissionClass::Authority),
];

/// The class of a built-in permission, or `None` when it is not in the catalog.
pub fn catalog_class(key: &str) -> Option<PermissionClass> {
    CATALOG.iter().find(|(k, _)| *k == key).map(|(_, c)| *c)
}

/// The surface part of `surface.action`.
pub fn surface_of(key: &str) -> &str {
    key.split('.').next().unwrap_or("")
}

/// True when `key` has the shape `surface.action` with lowercase ASCII
/// letters, digits and underscores in both parts.
pub fn is_permission_shape(key: &str) -> bool {
    let mut parts = key.split('.');
    let ok_part = |p: &str| {
        !p.is_empty()
            && p.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    matches!((parts.next(), parts.next(), parts.next()), (Some(a), Some(b), None) if ok_part(a) && ok_part(b))
}

/// Permissions an approver must hold to approve each class of change.
pub const APPROVE_GRANT: &str = "members.approve_grant";
pub const APPROVE_POLICY: &str = "policy.approve_change";
pub const PROPOSE_POLICY: &str = "policy.propose_change";
pub const GRANT: &str = "members.grant";
pub const REVOKE: &str = "members.revoke";
pub const SUSPEND: &str = "members.suspend";
pub const INVITE: &str = "members.invite";
/// The only permission a suspended member keeps.
pub const PROFILE_SELF: &str = "profile.self";
