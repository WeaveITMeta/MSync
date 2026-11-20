//! Standing and the Discord and Slack projections (hierarchy doc 4.4 and 4.5,
//! contract 12.2).
//!
//! ```text
//! rank(X)   = max(active position ranks of X, min(tier rank of X, merit_rank_ceiling))
//! effective = (tier perms filtered by policy) ∪ (∪ perms of active positions)
//!             minus the suspension mask (everything except profile.self)
//! ```
//!
//! This is the only place permissions are computed from a snapshot. Owners
//! (rank 1000) are engine configuration and are not part of a snapshot, so
//! they never appear here.

use std::collections::{BTreeMap, BTreeSet};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use super::catalog::{PermissionClass, PROFILE_SELF};
use super::model::{Grant, MemberStatus, Role, RoleKind};
use super::Snapshot;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierStanding {
    pub key: String,
    pub title: String,
    /// The tier's rank after the ceiling cap.
    pub rank: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PositionStanding {
    pub key: String,
    pub title: String,
    pub rank: u32,
    pub expires: Option<NaiveDate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Standing {
    pub member_id: String,
    pub status: MemberStatus,
    /// 0 unless the member is active.
    pub rank: u32,
    pub tier: Option<TierStanding>,
    /// Active (unexpired) positions, highest rank first.
    pub positions: Vec<PositionStanding>,
    /// Job titles, highest rank first: positions, then the tier.
    pub titles: Vec<String>,
    pub permissions: BTreeSet<String>,
    /// Discord role ids the member should hold. Empty unless active.
    pub discord_roles: Vec<String>,
    /// Slack user group ids the member should be in. Empty unless active.
    pub slack_usergroups: Vec<String>,
}

/// A grant counts while `today <= expires`. A grant without `expires` does
/// not expire.
pub fn grant_is_active(g: &Grant, today: NaiveDate) -> bool {
    g.expires.is_none_or(|e| today <= e)
}

/// Positions a member actively holds on `today`, highest rank first. Grants
/// that name an unknown role or a tier confer nothing.
pub fn active_positions<'a>(
    s: &'a Snapshot,
    member_id: &str,
    today: NaiveDate,
) -> Vec<(&'a Role, &'a Grant)> {
    let Some(m) = s.member(member_id) else {
        return Vec::new();
    };
    let mut out: Vec<(&Role, &Grant)> = m
        .grants
        .iter()
        .filter(|g| grant_is_active(g, today))
        .filter_map(|g| {
            s.role(&g.role)
                .filter(|r| r.kind == RoleKind::Position)
                .map(|r| (r, g))
        })
        .collect();
    out.sort_by(|a, b| b.0.rank.cmp(&a.0.rank).then(a.0.key.cmp(&b.0.key)));
    out
}

/// The highest tier whose `min_merit` the member meets.
pub fn tier_for(s: &Snapshot, vested_merit: u64) -> Option<&Role> {
    s.roles
        .roles
        .iter()
        .filter(|r| r.kind == RoleKind::Tier)
        .filter(|r| r.min_merit.is_some_and(|m| vested_merit >= m))
        .max_by(|a, b| a.rank.cmp(&b.rank).then(a.min_merit.cmp(&b.min_merit)))
}

/// A member's standing on `today` with `vested_merit` points. `None` when the
/// member id is not in the snapshot.
pub fn standing(
    s: &Snapshot,
    member_id: &str,
    vested_merit: u64,
    today: NaiveDate,
) -> Option<Standing> {
    let m = s.member(member_id)?;
    let ceiling = s.policy.promotion_merit_rank_ceiling;
    let tier = tier_for(s, vested_merit);
    let positions = active_positions(s, member_id, today);

    let tier_rank = tier.map(|t| t.rank.min(ceiling)).unwrap_or(0);
    let pos_rank = positions.iter().map(|(r, _)| r.rank).max().unwrap_or(0);

    let mut permissions = BTreeSet::new();
    if let Some(t) = tier {
        for p in &t.permissions {
            match s.permission_class(p) {
                // Read-time filter: even a bad snapshot cannot let merit grant authority.
                Some(PermissionClass::Authority)
                    if !s.policy.promotion_merit_may_confer_authority => {}
                Some(_) => {
                    permissions.insert(p.clone());
                }
                None => {}
            }
        }
    }
    for (r, _) in &positions {
        for p in &r.permissions {
            if s.permission_class(p).is_some() {
                permissions.insert(p.clone());
            }
        }
    }

    let mut discord_roles = Vec::new();
    for (r, _) in &positions {
        if let Some(d) = &r.discord_role {
            discord_roles.push(d.clone());
        }
    }
    if let Some(d) = tier.and_then(|t| t.discord_role.clone()) {
        discord_roles.push(d);
    }

    let mut slack_usergroups = Vec::new();
    for (r, _) in &positions {
        if let Some(g) = &r.slack_usergroup {
            slack_usergroups.push(g.clone());
        }
    }
    if let Some(g) = tier.and_then(|t| t.slack_usergroup.clone()) {
        slack_usergroups.push(g);
    }

    let mut titles: Vec<String> = positions.iter().map(|(r, _)| r.title.clone()).collect();
    if let Some(t) = tier {
        titles.push(t.title.clone());
    }

    let (rank, permissions, discord_roles, slack_usergroups) = match m.status {
        MemberStatus::Active => (
            pos_rank.max(tier_rank),
            permissions,
            discord_roles,
            slack_usergroups,
        ),
        MemberStatus::Suspended => {
            let mut only_self = permissions;
            only_self.retain(|p| p == PROFILE_SELF);
            (0, only_self, Vec::new(), Vec::new())
        }
        MemberStatus::Removed => (0, BTreeSet::new(), Vec::new(), Vec::new()),
    };

    Some(Standing {
        member_id: m.id.clone(),
        status: m.status,
        rank,
        tier: tier.map(|t| TierStanding {
            key: t.key.clone(),
            title: t.title.clone(),
            rank: t.rank.min(ceiling),
        }),
        positions: positions
            .iter()
            .map(|(r, g)| PositionStanding {
                key: r.key.clone(),
                title: r.title.clone(),
                rank: r.rank,
                expires: g.expires,
            })
            .collect(),
        titles,
        permissions,
        discord_roles,
        slack_usergroups,
    })
}

/// One managed Discord role, in hierarchy order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectedRole {
    pub discord_role_id: String,
    pub role_key: String,
    pub title: String,
    pub rank: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscordProjection {
    /// Every `(discord_user_id, discord_role_id)` pair that should exist.
    pub assignments: BTreeSet<(String, String)>,
    /// Managed roles from highest rank to lowest, so the job titles form the
    /// Discord role hierarchy. Ties break on the role key.
    pub role_order: Vec<ProjectedRole>,
}

/// The full Discord state the CRA asks for on `today`. `merits` maps member
/// id to vested merit; a member missing from it has 0. Members without a
/// Discord identity get nothing.
pub fn discord_projection(
    s: &Snapshot,
    merits: &BTreeMap<String, u64>,
    today: NaiveDate,
) -> DiscordProjection {
    let mut assignments = BTreeSet::new();
    for m in &s.members.members {
        let Some(user) = &m.identities.discord else {
            continue;
        };
        let merit = merits.get(&m.id).copied().unwrap_or(0);
        if let Some(st) = standing(s, &m.id, merit, today) {
            for role in st.discord_roles {
                assignments.insert((user.clone(), role));
            }
        }
    }
    let mut role_order: Vec<ProjectedRole> = s
        .roles
        .roles
        .iter()
        .filter_map(|r| {
            r.discord_role.as_ref().map(|d| ProjectedRole {
                discord_role_id: d.clone(),
                role_key: r.key.clone(),
                title: r.title.clone(),
                rank: r.rank,
            })
        })
        .collect();
    role_order.sort_by(|a, b| b.rank.cmp(&a.rank).then(a.role_key.cmp(&b.role_key)));
    DiscordProjection {
        assignments,
        role_order,
    }
}

/// One managed Slack user group, in hierarchy order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectedUsergroup {
    pub usergroup_id: String,
    pub role_key: String,
    pub title: String,
    pub rank: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackProjection {
    /// Every `(slack_user_id, usergroup_id)` pair that should exist.
    pub assignments: BTreeSet<(String, String)>,
    /// Managed user groups from highest rank to lowest. Ties break on the
    /// role key.
    pub usergroup_order: Vec<ProjectedUsergroup>,
}

impl SlackProjection {
    /// The full member list of every managed user group, sorted. A managed
    /// group nobody should be in maps to an empty list.
    pub fn users_by_usergroup(&self) -> BTreeMap<String, Vec<String>> {
        let mut out: BTreeMap<String, Vec<String>> = self
            .usergroup_order
            .iter()
            .map(|g| (g.usergroup_id.clone(), Vec::new()))
            .collect();
        for (user, group) in &self.assignments {
            out.entry(group.clone()).or_default().push(user.clone());
        }
        out
    }
}

/// The full Slack state the CRA asks for on `today`: the twin of
/// [`discord_projection`]. Slack has no roles, so a job title is membership
/// in the role's `slack_usergroup`. Members without a Slack identity get
/// nothing.
pub fn slack_projection(
    s: &Snapshot,
    merits: &BTreeMap<String, u64>,
    today: NaiveDate,
) -> SlackProjection {
    let mut assignments = BTreeSet::new();
    for m in &s.members.members {
        let Some(user) = &m.identities.slack else {
            continue;
        };
        let merit = merits.get(&m.id).copied().unwrap_or(0);
        if let Some(st) = standing(s, &m.id, merit, today) {
            for g in st.slack_usergroups {
                assignments.insert((user.clone(), g));
            }
        }
    }
    let mut usergroup_order: Vec<ProjectedUsergroup> = s
        .roles
        .roles
        .iter()
        .filter_map(|r| {
            r.slack_usergroup.as_ref().map(|g| ProjectedUsergroup {
                usergroup_id: g.clone(),
                role_key: r.key.clone(),
                title: r.title.clone(),
                rank: r.rank,
            })
        })
        .collect();
    usergroup_order.sort_by(|a, b| b.rank.cmp(&a.rank).then(a.role_key.cmp(&b.role_key)));
    SlackProjection {
        assignments,
        usergroup_order,
    }
}
