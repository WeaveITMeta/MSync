//! The change plan (contract section 4 step 3): what a new snapshot changes
//! relative to the active one, and the impact. The same function produces the
//! preview shown on a pull request before merge and the input to
//! [`super::authorize`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use chrono::NaiveDate;
use serde::Serialize;

use super::model::{Member, MemberStatus, RoleKind};
use super::policy::Mode;
use super::{FoundryIdentity, FoundryProvider, Snapshot};

/// One changed value. `None` means absent on that side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FieldChange {
    pub path: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModeChange {
    /// `None` for the first snapshot.
    pub old: Option<Mode>,
    pub new: Mode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoleSummary {
    pub key: String,
    pub title: String,
    pub kind: RoleKind,
    pub rank: u32,
    /// Members holding a grant of this role in either snapshot.
    pub holders: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoleChange {
    pub key: String,
    pub title: String,
    pub old_rank: u32,
    pub new_rank: u32,
    pub changes: Vec<FieldChange>,
    /// Members holding a grant of this role in either snapshot.
    pub holders: Vec<String>,
}

impl RoleChange {
    /// The rank that governs the change: old or new, whichever is higher.
    pub fn rank(&self) -> u32 {
        self.old_rank.max(self.new_rank)
    }
}

/// The member a change is about, with the facts authorization needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemberTarget {
    pub id: String,
    pub name: String,
    /// The rank that protects this member: the highest rank of any grant the
    /// member holds in either snapshot (expired or not, each role at its
    /// higher rank of old and new), and never below the merit rank ceiling of
    /// either snapshot, because merit is not visible here. Owners are raised
    /// to 1000 by [`super::authorize`], which knows the owner list.
    pub rank: u32,
    /// GitHub and GitLab identities of the member in either snapshot.
    pub foundry_identities: Vec<FoundryIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GrantDelta {
    pub member: MemberTarget,
    pub role: String,
    /// Rank of the role, old or new, whichever is higher.
    pub role_rank: u32,
    pub granted_by: String,
    pub expires: Option<NaiveDate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GrantChange {
    pub member: MemberTarget,
    pub role: String,
    pub role_rank: u32,
    pub old_expires: Option<NaiveDate>,
    pub new_expires: Option<NaiveDate>,
    pub old_granted_by: String,
    pub new_granted_by: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusChange {
    pub member: MemberTarget,
    pub old: MemberStatus,
    pub new: MemberStatus,
}

/// A change to a member's record: an identity (`github`, `gitlab`,
/// `discord`, `slack`) or the display `name`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IdentityChange {
    pub member: MemberTarget,
    pub field: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemberDelta {
    pub member: MemberTarget,
    pub status: MemberStatus,
    /// `(provider, value)` of every identity on the added or removed entry.
    pub identities: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Impact {
    /// Discord role id to the members who would gain it through a position.
    pub discord_role_gains: BTreeMap<String, Vec<String>>,
    /// Discord role id to the members who would lose it through a position.
    pub discord_role_losses: BTreeMap<String, Vec<String>>,
    /// Slack user group id to the members who would join it through a position.
    pub slack_usergroup_gains: BTreeMap<String, Vec<String>>,
    /// Slack user group id to the members who would leave it through a position.
    pub slack_usergroup_losses: BTreeMap<String, Vec<String>>,
    /// The new snapshot's `promotion.merit_rank_ceiling`.
    pub new_merit_rank_ceiling: u32,
    /// Tiers (in either snapshot, at their higher rank) whose rank exceeds
    /// the new ceiling: `(key, rank)`.
    pub tiers_exceeding_new_ceiling: Vec<(String, u32)>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChangePlan {
    /// `None` for the first snapshot of an organization.
    pub old_hash: Option<String>,
    pub new_hash: String,
    pub org_slug: String,
    pub mode_change: Option<ModeChange>,
    /// Effective policy fields that change, keyed by the override key.
    pub policy_changes: Vec<FieldChange>,
    /// Other `org.toml` changes: name, discord, slack, `[[foundry]]` repositories, consumers,
    /// compartments, permissions_extra, template.
    pub org_changes: Vec<FieldChange>,
    pub roles_added: Vec<RoleSummary>,
    pub roles_removed: Vec<RoleSummary>,
    pub roles_changed: Vec<RoleChange>,
    pub members_added: Vec<MemberDelta>,
    pub members_removed: Vec<MemberDelta>,
    pub grants_added: Vec<GrantDelta>,
    pub grants_removed: Vec<GrantDelta>,
    pub grants_changed: Vec<GrantChange>,
    pub status_changes: Vec<StatusChange>,
    pub identity_changes: Vec<IdentityChange>,
    pub merit_changes: Vec<FieldChange>,
    pub payout_changes: Vec<FieldChange>,
    /// The lower of the old and new `grants.two_person_min_rank`: a change at
    /// or above it needs two people. Taking the lower one means a commit
    /// cannot raise the threshold to escape its own two-person check.
    pub two_person_min_rank: u32,
    /// The rank an org-level change (mode, policy, other org.toml, merit,
    /// payouts) must be strictly above: the higher of the old and new
    /// `grants.two_person_min_rank`.
    pub org_change_rank: u32,
    pub impact: Impact,
    /// The snapshot being planned. Authorization never resolves an actor or
    /// approver through it.
    #[serde(skip)]
    pub new: Snapshot,
}

impl ChangePlan {
    /// True when nothing changes.
    pub fn is_empty(&self) -> bool {
        self.mode_change.is_none()
            && self.policy_changes.is_empty()
            && self.org_changes.is_empty()
            && self.roles_added.is_empty()
            && self.roles_removed.is_empty()
            && self.roles_changed.is_empty()
            && self.members_added.is_empty()
            && self.members_removed.is_empty()
            && self.grants_added.is_empty()
            && self.grants_removed.is_empty()
            && self.grants_changed.is_empty()
            && self.status_changes.is_empty()
            && self.identity_changes.is_empty()
            && self.merit_changes.is_empty()
            && self.payout_changes.is_empty()
    }
}

// ------------------------------------------------------------- flattening

const ID_FIELDS: &[&str] = &["key", "repo", "branch", "id", "role"];

fn render(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Flattens a TOML value into `path -> rendered value`. Arrays of tables are
/// indexed by their identifying field (`key`, `repo`, `branch`, `id`,
/// `role`) when present, so reordering entries is not a change.
pub fn flatten(prefix: &str, v: &toml::Value, out: &mut BTreeMap<String, String>) {
    let join = |k: &str| if prefix.is_empty() { k.to_string() } else { format!("{prefix}.{k}") };
    match v {
        toml::Value::Table(t) => {
            for (k, v) in t {
                flatten(&join(k), v, out);
            }
        }
        toml::Value::Array(items) if items.iter().any(|i| i.is_table()) => {
            for (i, item) in items.iter().enumerate() {
                let id = item
                    .as_table()
                    .and_then(|t| ID_FIELDS.iter().find_map(|f| t.get(*f).and_then(|x| x.as_str())))
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| i.to_string());
                flatten(&format!("{prefix}[{id}]"), item, out);
            }
        }
        other => {
            out.insert(prefix.to_string(), render(other));
        }
    }
}

fn flat_of<T: Serialize>(x: &T) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Ok(v) = toml::Value::try_from(x) {
        flatten("", &v, &mut out);
    }
    out
}

fn diff_maps(old: &BTreeMap<String, String>, new: &BTreeMap<String, String>) -> Vec<FieldChange> {
    let keys: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    keys.into_iter()
        .filter_map(|k| {
            let (o, n) = (old.get(k), new.get(k));
            (o != n).then(|| FieldChange { path: k.clone(), old: o.cloned(), new: n.cloned() })
        })
        .collect()
}

fn org_flat(s: &Snapshot) -> BTreeMap<String, String> {
    let mut m = flat_of(&s.org);
    m.retain(|k, _| k != "schema" && k.strip_prefix("org.") != Some("mode") && k != "policy" && !k.starts_with("policy."));
    m
}

fn file_flat<T: Serialize>(x: &T) -> BTreeMap<String, String> {
    let mut m = flat_of(x);
    m.remove("schema");
    m
}

// --------------------------------------------------------------- planning

fn holders(snaps: &[&Snapshot], role: &str) -> Vec<String> {
    let mut out = BTreeSet::new();
    for s in snaps {
        for m in &s.members.members {
            if m.grants.iter().any(|g| g.role == role) {
                out.insert(m.id.clone());
            }
        }
    }
    out.into_iter().collect()
}

fn role_rank_either(snaps: &[&Snapshot], role: &str) -> u32 {
    snaps.iter().filter_map(|s| s.role(role)).map(|r| r.rank).max().unwrap_or(0)
}

fn foundry_ids(m: &Member) -> Vec<FoundryIdentity> {
    let mut v = Vec::new();
    if let Some(l) = &m.identities.github {
        v.push(FoundryIdentity { provider: FoundryProvider::Github, login: l.clone() });
    }
    if let Some(l) = &m.identities.gitlab {
        v.push(FoundryIdentity { provider: FoundryProvider::Gitlab, login: l.clone() });
    }
    v
}

fn target(snaps: &[&Snapshot], id: &str) -> MemberTarget {
    let mut rank = snaps.iter().map(|s| s.policy.promotion_merit_rank_ceiling).max().unwrap_or(0);
    let mut name = String::new();
    let mut ids: Vec<FoundryIdentity> = Vec::new();
    for s in snaps {
        if let Some(m) = s.member(id) {
            name = m.name.clone();
            for g in &m.grants {
                rank = rank.max(role_rank_either(snaps, &g.role));
            }
            for fi in foundry_ids(m) {
                if !ids.iter().any(|x| x.matches(fi.provider, &fi.login)) {
                    ids.push(fi);
                }
            }
        }
    }
    MemberTarget { id: id.to_string(), name, rank, foundry_identities: ids }
}

/// `(projected id, member id)` for every active position grant, where the
/// projected id is what `pick` reads from the role (its Discord role or its
/// Slack user group).
fn position_pairs(s: &Snapshot, pick: fn(&super::model::Role) -> Option<&String>) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    for m in &s.members.members {
        if m.status != MemberStatus::Active {
            continue;
        }
        for g in &m.grants {
            if let Some(d) = s.role(&g.role).filter(|r| r.kind == RoleKind::Position).and_then(|r| pick(r).cloned()) {
                out.insert((d, m.id.clone()));
            }
        }
    }
    out
}

fn discord_of(r: &super::model::Role) -> Option<&String> {
    r.discord_role.as_ref()
}

fn slack_of(r: &super::model::Role) -> Option<&String> {
    r.slack_usergroup.as_ref()
}

/// Diffs `new` against the active snapshot `old` (`None` for the first
/// snapshot of an organization).
pub fn plan_change(old: Option<&Snapshot>, new: &Snapshot) -> ChangePlan {
    let snaps: Vec<&Snapshot> = old.into_iter().chain(std::iter::once(new)).collect();
    let empty_flat = BTreeMap::new();

    // Mode and policy.
    let mode_change = match old {
        Some(o) if o.mode_label == new.mode_label => None,
        Some(o) => Some(ModeChange { old: Some(o.mode_label), new: new.mode_label }),
        None => Some(ModeChange { old: None, new: new.mode_label }),
    };
    let new_policy: BTreeMap<String, String> =
        new.policy.entries().into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    let old_policy: BTreeMap<String, String> = old
        .map(|o| o.policy.entries().into_iter().map(|(k, v)| (k.to_string(), v)).collect())
        .unwrap_or_default();
    let policy_changes = diff_maps(&old_policy, &new_policy);

    let org_changes = diff_maps(old.map(org_flat).as_ref().unwrap_or(&empty_flat), &org_flat(new));
    let merit_changes = diff_maps(old.map(|o| file_flat(&o.merit)).as_ref().unwrap_or(&empty_flat), &file_flat(&new.merit));
    let payout_changes =
        diff_maps(old.map(|o| file_flat(&o.payouts)).as_ref().unwrap_or(&empty_flat), &file_flat(&new.payouts));

    // Roles.
    let mut roles_added = Vec::new();
    let mut roles_removed = Vec::new();
    let mut roles_changed = Vec::new();
    for r in &new.roles.roles {
        match old.and_then(|o| o.role(&r.key)) {
            None => roles_added.push(RoleSummary {
                key: r.key.clone(),
                title: r.title.clone(),
                kind: r.kind,
                rank: r.rank,
                holders: holders(&snaps, &r.key),
            }),
            Some(o) if o != r => roles_changed.push(RoleChange {
                key: r.key.clone(),
                title: r.title.clone(),
                old_rank: o.rank,
                new_rank: r.rank,
                changes: diff_maps(&flat_of(o), &flat_of(r)),
                holders: holders(&snaps, &r.key),
            }),
            Some(_) => {}
        }
    }
    if let Some(o) = old {
        for r in &o.roles.roles {
            if new.role(&r.key).is_none() {
                roles_removed.push(RoleSummary {
                    key: r.key.clone(),
                    title: r.title.clone(),
                    kind: r.kind,
                    rank: r.rank,
                    holders: holders(&snaps, &r.key),
                });
            }
        }
    }

    // Members, grants, status, identities.
    let mut members_added = Vec::new();
    let mut members_removed = Vec::new();
    let mut grants_added = Vec::new();
    let mut grants_removed = Vec::new();
    let mut grants_changed = Vec::new();
    let mut status_changes = Vec::new();
    let mut identity_changes = Vec::new();

    let ids_of = |m: &Member| m.identities.entries().into_iter().map(|(p, v)| (p.to_string(), v.to_string())).collect::<Vec<_>>();
    let grant_delta = |snaps: &[&Snapshot], m: &Member, g: &super::model::Grant| GrantDelta {
        member: target(snaps, &m.id),
        role: g.role.clone(),
        role_rank: role_rank_either(snaps, &g.role),
        granted_by: g.granted_by.clone(),
        expires: g.expires,
    };

    for m in &new.members.members {
        let prev = old.and_then(|o| o.member(&m.id));
        match prev {
            None => {
                members_added.push(MemberDelta { member: target(&snaps, &m.id), status: m.status, identities: ids_of(m) });
                for g in &m.grants {
                    grants_added.push(grant_delta(&snaps, m, g));
                }
            }
            Some(p) => {
                if p.status != m.status {
                    status_changes.push(StatusChange { member: target(&snaps, &m.id), old: p.status, new: m.status });
                }
                let pairs = [
                    ("github", &p.identities.github, &m.identities.github),
                    ("gitlab", &p.identities.gitlab, &m.identities.gitlab),
                    ("discord", &p.identities.discord, &m.identities.discord),
                    ("slack", &p.identities.slack, &m.identities.slack),
                ];
                for (field, a, b) in pairs {
                    if a != b {
                        identity_changes.push(IdentityChange {
                            member: target(&snaps, &m.id),
                            field: field.to_string(),
                            old: a.clone(),
                            new: b.clone(),
                        });
                    }
                }
                if p.name != m.name {
                    identity_changes.push(IdentityChange {
                        member: target(&snaps, &m.id),
                        field: "name".into(),
                        old: Some(p.name.clone()),
                        new: Some(m.name.clone()),
                    });
                }
                for g in &m.grants {
                    match p.grants.iter().find(|x| x.role == g.role) {
                        None => grants_added.push(grant_delta(&snaps, m, g)),
                        Some(x) if x != g => grants_changed.push(GrantChange {
                            member: target(&snaps, &m.id),
                            role: g.role.clone(),
                            role_rank: role_rank_either(&snaps, &g.role),
                            old_expires: x.expires,
                            new_expires: g.expires,
                            old_granted_by: x.granted_by.clone(),
                            new_granted_by: g.granted_by.clone(),
                        }),
                        Some(_) => {}
                    }
                }
                for x in &p.grants {
                    if !m.grants.iter().any(|g| g.role == x.role) {
                        grants_removed.push(grant_delta(&snaps, p, x));
                    }
                }
            }
        }
    }
    if let Some(o) = old {
        for p in &o.members.members {
            if new.member(&p.id).is_none() {
                members_removed.push(MemberDelta { member: target(&snaps, &p.id), status: p.status, identities: ids_of(p) });
                for x in &p.grants {
                    grants_removed.push(grant_delta(&snaps, p, x));
                }
            }
        }
    }

    // Thresholds.
    let tps: Vec<u32> = snaps.iter().map(|s| s.policy.grants_two_person_min_rank).collect();
    let two_person_min_rank = tps.iter().copied().min().unwrap_or(0);
    let org_change_rank = tps.iter().copied().max().unwrap_or(0);

    // Impact.
    let old_pairs = old.map(|o| position_pairs(o, discord_of)).unwrap_or_default();
    let new_pairs = position_pairs(new, discord_of);
    let mut impact = Impact { new_merit_rank_ceiling: new.policy.promotion_merit_rank_ceiling, ..Default::default() };
    for (d, m) in new_pairs.difference(&old_pairs) {
        impact.discord_role_gains.entry(d.clone()).or_default().push(m.clone());
    }
    for (d, m) in old_pairs.difference(&new_pairs) {
        impact.discord_role_losses.entry(d.clone()).or_default().push(m.clone());
    }
    let old_groups = old.map(|o| position_pairs(o, slack_of)).unwrap_or_default();
    let new_groups = position_pairs(new, slack_of);
    for (g, m) in new_groups.difference(&old_groups) {
        impact.slack_usergroup_gains.entry(g.clone()).or_default().push(m.clone());
    }
    for (g, m) in old_groups.difference(&new_groups) {
        impact.slack_usergroup_losses.entry(g.clone()).or_default().push(m.clone());
    }
    let mut tiers: BTreeMap<String, u32> = BTreeMap::new();
    for s in &snaps {
        for r in s.roles.roles.iter().filter(|r| r.kind == RoleKind::Tier) {
            let e = tiers.entry(r.key.clone()).or_insert(0);
            *e = (*e).max(r.rank);
        }
    }
    impact.tiers_exceeding_new_ceiling =
        tiers.into_iter().filter(|(_, rank)| *rank > impact.new_merit_rank_ceiling).collect();

    ChangePlan {
        old_hash: old.map(|o| o.content_hash.clone()),
        new_hash: new.content_hash.clone(),
        org_slug: new.org.org.slug.clone(),
        mode_change,
        policy_changes,
        org_changes,
        roles_added,
        roles_removed,
        roles_changed,
        members_added,
        members_removed,
        grants_added,
        grants_removed,
        grants_changed,
        status_changes,
        identity_changes,
        merit_changes,
        payout_changes,
        two_person_min_rank,
        org_change_rank,
        impact,
        new: new.clone(),
    }
}

// -------------------------------------------------------------- rendering

fn opt(v: &Option<String>) -> String {
    v.as_deref().map(|s| format!("`{s}`")).unwrap_or_else(|| "(none)".into())
}

fn date(d: &Option<NaiveDate>) -> String {
    d.map(|d| d.to_string()).unwrap_or_else(|| "never".into())
}

fn field_table(out: &mut String, title: &str, rows: &[FieldChange]) {
    if rows.is_empty() {
        return;
    }
    let _ = writeln!(out, "### {title}\n\n| Field | Old | New |\n|---|---|---|");
    for r in rows {
        let _ = writeln!(out, "| `{}` | {} | {} |", r.path, opt(&r.old), opt(&r.new));
    }
    out.push('\n');
}

impl ChangePlan {
    /// The preview shown before a mode switch or any other change is
    /// applied: every change, the impact, and what authority each change
    /// needs.
    pub fn render_markdown(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "## MSync CRA change preview: `{}`\n", self.org_slug);
        match &self.old_hash {
            Some(h) => {
                let _ = writeln!(out, "- Active snapshot: `{h}`");
            }
            None => {
                let _ = writeln!(out, "- Active snapshot: none. This is the first snapshot; only an owner may load it.");
            }
        }
        let _ = writeln!(out, "- New snapshot: `{}`", self.new_hash);
        let _ = writeln!(
            out,
            "- Two-person threshold for this change: rank {}. Org-level changes need rank above {}.\n",
            self.two_person_min_rank, self.org_change_rank
        );
        if self.is_empty() {
            out.push_str("No changes.\n");
            return out;
        }

        if let Some(m) = &self.mode_change {
            let _ = writeln!(
                out,
                "### Mode\n\n`{}` to `{}`\n",
                m.old.map(|x| x.as_str()).unwrap_or("(none)"),
                m.new.as_str()
            );
        }
        field_table(&mut out, "Policy (effective values)", &self.policy_changes);
        field_table(&mut out, "Organization", &self.org_changes);

        if !self.roles_added.is_empty() || !self.roles_removed.is_empty() || !self.roles_changed.is_empty() {
            let _ = writeln!(out, "### Roles\n");
            for r in &self.roles_added {
                let _ = writeln!(out, "- Added {:?} `{}` \"{}\" rank {}", r.kind, r.key, r.title, r.rank);
            }
            for r in &self.roles_removed {
                let _ = writeln!(
                    out,
                    "- Removed {:?} `{}` \"{}\" rank {} (holders: {})",
                    r.kind,
                    r.key,
                    r.title,
                    r.rank,
                    if r.holders.is_empty() { "none".into() } else { r.holders.join(", ") }
                );
            }
            for r in &self.roles_changed {
                let _ = writeln!(out, "- Changed `{}` \"{}\" rank {} to {}", r.key, r.title, r.old_rank, r.new_rank);
                for c in &r.changes {
                    let _ = writeln!(out, "  - `{}`: {} to {}", c.path, opt(&c.old), opt(&c.new));
                }
            }
            out.push('\n');
        }

        let any_members = !self.members_added.is_empty()
            || !self.members_removed.is_empty()
            || !self.status_changes.is_empty()
            || !self.identity_changes.is_empty();
        if any_members {
            let _ = writeln!(out, "### Members\n");
            for m in &self.members_added {
                let ids: Vec<String> = m.identities.iter().map(|(p, v)| format!("{p}:{v}")).collect();
                let _ = writeln!(out, "- Added `{}` ({}), status {}, identities [{}]", m.member.id, m.member.name, m.status.as_str(), ids.join(", "));
            }
            for m in &self.members_removed {
                let _ = writeln!(out, "- Removed `{}` ({})", m.member.id, m.member.name);
            }
            for s in &self.status_changes {
                let _ = writeln!(out, "- Status of `{}`: {} to {}", s.member.id, s.old.as_str(), s.new.as_str());
            }
            for i in &self.identity_changes {
                let _ = writeln!(out, "- `{}` {}: {} to {}", i.member.id, i.field, opt(&i.old), opt(&i.new));
            }
            out.push('\n');
        }

        if !self.grants_added.is_empty() || !self.grants_removed.is_empty() || !self.grants_changed.is_empty() {
            let _ = writeln!(out, "### Grants\n");
            for g in &self.grants_added {
                let _ = writeln!(out, "- Grant `{}` (rank {}) to `{}`, granted_by `{}`, expires {}", g.role, g.role_rank, g.member.id, g.granted_by, date(&g.expires));
            }
            for g in &self.grants_removed {
                let _ = writeln!(out, "- Revoke `{}` (rank {}) from `{}`", g.role, g.role_rank, g.member.id);
            }
            for g in &self.grants_changed {
                let _ = writeln!(
                    out,
                    "- Change `{}` of `{}`: expires {} to {}, granted_by `{}` to `{}`",
                    g.role,
                    g.member.id,
                    date(&g.old_expires),
                    date(&g.new_expires),
                    g.old_granted_by,
                    g.new_granted_by
                );
            }
            out.push('\n');
        }

        field_table(&mut out, "Merit (merit.toml)", &self.merit_changes);
        field_table(&mut out, "Payouts (payouts.toml)", &self.payout_changes);

        let _ = writeln!(out, "### Impact\n");
        if self.impact.discord_role_gains.is_empty() && self.impact.discord_role_losses.is_empty() {
            let _ = writeln!(out, "- No member gains or loses a position Discord role.");
        }
        for (d, ms) in &self.impact.discord_role_gains {
            let _ = writeln!(out, "- Discord role `{d}`: {} member(s) gain it ({})", ms.len(), ms.join(", "));
        }
        for (d, ms) in &self.impact.discord_role_losses {
            let _ = writeln!(out, "- Discord role `{d}`: {} member(s) lose it ({})", ms.len(), ms.join(", "));
        }
        if self.impact.slack_usergroup_gains.is_empty() && self.impact.slack_usergroup_losses.is_empty() {
            let _ = writeln!(out, "- No member joins or leaves a position Slack user group.");
        }
        for (g, ms) in &self.impact.slack_usergroup_gains {
            let _ = writeln!(out, "- Slack user group `{g}`: {} member(s) join it ({})", ms.len(), ms.join(", "));
        }
        for (g, ms) in &self.impact.slack_usergroup_losses {
            let _ = writeln!(out, "- Slack user group `{g}`: {} member(s) leave it ({})", ms.len(), ms.join(", "));
        }
        if self.impact.tiers_exceeding_new_ceiling.is_empty() {
            let _ = writeln!(out, "- No tier exceeds the new merit rank ceiling ({}).", self.impact.new_merit_rank_ceiling);
        } else {
            let list: Vec<String> =
                self.impact.tiers_exceeding_new_ceiling.iter().map(|(k, r)| format!("`{k}` (rank {r})")).collect();
            let _ = writeln!(
                out,
                "- {} tier(s) exceed the new merit rank ceiling {}: {}. They must be lowered or removed, or the snapshot is refused.",
                list.len(),
                self.impact.new_merit_rank_ceiling,
                list.join(", ")
            );
        }
        let _ = writeln!(out, "- Tier Discord roles and Slack user groups depend on merit and are recomputed at reconciliation; they are not counted here.\n");

        let reqs = super::authz::requirements(self);
        if !reqs.is_empty() {
            let _ = writeln!(out, "### Authority needed\n\n| Change | Actor needs | Rank above | Two-person |\n|---|---|---|---|");
            for r in reqs {
                let _ = writeln!(
                    out,
                    "| {} | `{}` | {} | {} |",
                    r.item.replace('|', "\\|"),
                    r.permission,
                    r.rank,
                    if r.two_person { format!("yes, approver needs `{}`", r.approver_permission) } else { "no".into() }
                );
            }
            out.push('\n');
        }
        out
    }
}
