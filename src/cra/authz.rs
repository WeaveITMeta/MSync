//! Change authorization (contract section 4 step 4).
//!
//! A change is authorized against the ACTIVE snapshot, never the new one: the
//! actor and every approver are resolved to members through the active
//! snapshot's identities, and their rank and permissions come from the active
//! snapshot. A change cannot authorize itself.
//!
//! Rules, all strict (`>`, never `>=`):
//!
//! - Grant added or changed: actor rank > role rank, `members.grant`.
//! - Grant removed: actor rank > max(role rank, member rank), `members.revoke`.
//! - Role created, deleted or changed: actor rank > max(old rank, new rank),
//!   `policy.propose_change`.
//! - Member status changed or member entry removed: actor rank > member rank,
//!   `members.suspend`. Member entry added: `members.invite`.
//! - Identity or name changed: actor rank > member rank, `members.grant` (the
//!   authority to change the member's highest grant).
//! - Mode, policy, other org.toml, merit.toml, payouts.toml: actor rank >
//!   `org_change_rank`, `policy.propose_change`, always two-person.
//! - Nobody changes their own grants, status, identities or member entry.
//! - Two-person (any of the above at or above `two_person_min_rank`, and every
//!   org-level change): a merged pull request with an approver who is not the
//!   author, not the actor, not an affected member, is active, holds the
//!   approval permission and passes the same strict rank rule. Direct pushes
//!   carry no approver.
//! - Bootstrap: with no active snapshot, only an owner may load the first one.

use std::collections::BTreeSet;
use std::fmt;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use super::catalog::{APPROVE_GRANT, APPROVE_POLICY, GRANT, INVITE, PROPOSE_POLICY, REVOKE, SUSPEND};
use super::model::MemberStatus;
use super::plan::{ChangePlan, MemberTarget};
use super::standing::active_positions;
use super::{ForgeIdentity, ForgeProvider, Snapshot};

/// The rank of an owner. Owners come from engine configuration only.
pub const OWNER_RANK: u32 = 1000;

/// Who made the change and how it arrived.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeContext {
    /// The forge-authenticated identity that merged or pushed.
    pub actor: ForgeIdentity,
    /// True only for a merged pull or merge request.
    pub via_pull_request: bool,
    /// The pull request author. Required for a two-person change.
    pub author: Option<ForgeIdentity>,
    /// Identities that left an approving review. Ignored on a direct push.
    pub approvers: Vec<ForgeIdentity>,
    /// The date authority is evaluated on: expired grants confer nothing.
    pub today: NaiveDate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ViolationCode {
    /// The plan was not computed against this active snapshot.
    PlanMismatch,
    /// First snapshot loaded by someone who is not an owner.
    NotBootstrapOwner,
    /// The actor is not a member of the active snapshot and not an owner.
    UnknownActor,
    /// The actor's member entry is suspended or removed.
    ActorNotActive,
    /// The actor's rank is not strictly above the rank the change needs.
    InsufficientRank,
    /// The actor lacks the permission the change needs.
    MissingPermission,
    /// The actor changes their own grants, status, identities or entry.
    SelfChange,
    /// A two-person change arrived by direct push.
    DirectPushNeedsApproval,
    /// A two-person change arrived by pull request with no known author.
    PullRequestAuthorUnknown,
    /// No approver satisfies every approver rule.
    NoQualifiedApprover,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Violation {
    pub code: ViolationCode,
    /// The change the violation is about.
    pub item: String,
    pub message: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}: {}", self.code, self.item, self.message)
    }
}

/// What one change in a plan needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Requirement {
    pub item: String,
    /// Actor and approvers must be strictly above this rank.
    pub rank: u32,
    pub permission: &'static str,
    pub two_person: bool,
    pub approver_permission: &'static str,
    /// The member the change is about, when it is about one member. The
    /// actor may not be this member; owners are raised to rank 1000.
    pub target: Option<MemberTarget>,
    /// Members who benefit or are affected. No approver may be one of them.
    pub affected: Vec<MemberTarget>,
}

/// One requirement per change in the plan. The plan preview prints these, and
/// [`authorize`] checks them; there is no second copy of the rules.
pub fn requirements(plan: &ChangePlan) -> Vec<Requirement> {
    let tp = plan.two_person_min_rank;
    let org_rank = plan.org_change_rank;
    let mut out = Vec::new();
    let org = |item: String| Requirement {
        item,
        rank: org_rank,
        permission: PROPOSE_POLICY,
        two_person: true,
        approver_permission: APPROVE_POLICY,
        target: None,
        affected: Vec::new(),
    };

    if let Some(m) = &plan.mode_change {
        if m.old.is_some() {
            out.push(org(format!(
                "mode {} to {}",
                m.old.map(|x| x.as_str()).unwrap_or("(none)"),
                m.new.as_str()
            )));
        }
    }
    if plan.old_hash.is_some() {
        for c in &plan.policy_changes {
            out.push(org(format!("policy `{}`", c.path)));
        }
    }
    for c in &plan.org_changes {
        out.push(org(format!("org.toml `{}`", c.path)));
    }
    for c in &plan.merit_changes {
        out.push(org(format!("merit.toml `{}`", c.path)));
    }
    for c in &plan.payout_changes {
        out.push(org(format!("payouts.toml `{}`", c.path)));
    }

    let holders_of = |ids: &[String]| -> Vec<MemberTarget> {
        ids.iter()
            .map(|id| plan_target(plan, id))
            .collect()
    };
    let role_req = |item: String, rank: u32, holders: &[String]| Requirement {
        item,
        rank,
        permission: PROPOSE_POLICY,
        two_person: rank >= tp,
        approver_permission: APPROVE_POLICY,
        target: None,
        affected: holders_of(holders),
    };
    for r in &plan.roles_added {
        out.push(role_req(format!("add role `{}` (rank {})", r.key, r.rank), r.rank, &r.holders));
    }
    for r in &plan.roles_removed {
        out.push(role_req(format!("remove role `{}` (rank {})", r.key, r.rank), r.rank, &r.holders));
    }
    for r in &plan.roles_changed {
        out.push(role_req(format!("change role `{}` (rank {} to {})", r.key, r.old_rank, r.new_rank), r.rank(), &r.holders));
    }

    let member_req = |item: String, rank: u32, permission: &'static str, t: &MemberTarget| Requirement {
        item,
        rank,
        permission,
        two_person: rank >= tp,
        approver_permission: APPROVE_GRANT,
        target: Some(t.clone()),
        affected: vec![t.clone()],
    };
    for g in &plan.grants_added {
        out.push(member_req(format!("grant `{}` to `{}`", g.role, g.member.id), g.role_rank, GRANT, &g.member));
    }
    for g in &plan.grants_changed {
        out.push(member_req(format!("change grant `{}` of `{}`", g.role, g.member.id), g.role_rank, GRANT, &g.member));
    }
    for g in &plan.grants_removed {
        out.push(member_req(
            format!("revoke `{}` from `{}`", g.role, g.member.id),
            g.role_rank.max(g.member.rank),
            REVOKE,
            &g.member,
        ));
    }
    for s in &plan.status_changes {
        out.push(member_req(
            format!("status of `{}` {} to {}", s.member.id, s.old.as_str(), s.new.as_str()),
            s.member.rank,
            SUSPEND,
            &s.member,
        ));
    }
    for m in &plan.members_added {
        out.push(member_req(format!("add member `{}`", m.member.id), m.member.rank, INVITE, &m.member));
    }
    for m in &plan.members_removed {
        out.push(member_req(format!("remove member `{}`", m.member.id), m.member.rank, SUSPEND, &m.member));
    }
    for i in &plan.identity_changes {
        out.push(member_req(format!("`{}` {} change", i.member.id, i.field), i.member.rank, GRANT, &i.member));
    }
    out
}

fn plan_target(plan: &ChangePlan, id: &str) -> MemberTarget {
    let mut ids = Vec::new();
    let mut name = String::new();
    if let Some(m) = plan.new.member(id) {
        name = m.name.clone();
        if let Some(l) = &m.identities.github {
            ids.push(ForgeIdentity::github(l));
        }
        if let Some(l) = &m.identities.gitlab {
            ids.push(ForgeIdentity::gitlab(l));
        }
    }
    MemberTarget { id: id.to_string(), name, rank: 0, forge_identities: ids }
}

/// An actor or approver after resolution through the active snapshot.
#[derive(Debug, Clone)]
struct Principal {
    identity: ForgeIdentity,
    member_id: Option<String>,
    owner: bool,
    active: bool,
    rank: u32,
    permissions: BTreeSet<String>,
}

impl Principal {
    fn has(&self, perm: &str) -> bool {
        self.owner || self.permissions.contains(perm)
    }
    fn is(&self, t: &MemberTarget) -> bool {
        self.member_id.as_deref() == Some(t.id.as_str())
            || t.forge_identities.iter().any(|f| self.identity.matches(f.provider, &f.login))
    }
    fn same_person(&self, other: &Principal) -> bool {
        self.identity.matches(other.identity.provider, &other.identity.login)
            || (self.member_id.is_some() && self.member_id == other.member_id)
    }
}

/// The strict rank rule. One function, so the rule cannot drift.
fn outranks(actor_rank: u32, required: u32) -> bool {
    actor_rank > required
}

/// Owner entries are either a member id (`mckale`) or a forge identity
/// (`github:Simbuilder`, `gitlab:someone`). Only the forge form can load the
/// first snapshot, because a member id means nothing before one exists.
fn owner_forge(entry: &str) -> Option<ForgeIdentity> {
    let (p, login) = entry.split_once(':')?;
    let provider = match p {
        "github" => ForgeProvider::Github,
        "gitlab" => ForgeProvider::Gitlab,
        _ => return None,
    };
    Some(ForgeIdentity { provider, login: login.to_string() })
}

fn is_owner_identity(who: &ForgeIdentity, owners: &[String]) -> bool {
    owners.iter().filter_map(|o| owner_forge(o)).any(|o| who.matches(o.provider, &o.login))
}

fn is_owner_member(id: &str, owners: &[String]) -> bool {
    owners.iter().any(|o| !o.contains(':') && o == id)
}

fn target_is_owner(t: &MemberTarget, owners: &[String]) -> bool {
    is_owner_member(&t.id, owners) || t.forge_identities.iter().any(|f| is_owner_identity(f, owners))
}

/// Resolves an identity through the ACTIVE snapshot (`roster`).
fn resolve(roster: &Snapshot, who: &ForgeIdentity, owners: &[String], today: NaiveDate) -> Option<Principal> {
    let member = roster.member_by_forge(who);
    let owner = is_owner_identity(who, owners) || member.map_or(false, |m| is_owner_member(&m.id, owners));
    if owner {
        return Some(Principal {
            identity: who.clone(),
            member_id: member.map(|m| m.id.clone()),
            owner: true,
            active: true,
            rank: OWNER_RANK,
            permissions: BTreeSet::new(),
        });
    }
    let m = member?;
    let active = m.status == MemberStatus::Active;
    let positions = active_positions(roster, &m.id, today);
    let rank = if active { positions.iter().map(|(r, _)| r.rank).max().unwrap_or(0) } else { 0 };
    let permissions = if active {
        positions.iter().flat_map(|(r, _)| r.permissions.iter().cloned()).collect()
    } else {
        BTreeSet::new()
    };
    Some(Principal { identity: who.clone(), member_id: Some(m.id.clone()), owner: false, active, rank, permissions })
}

/// Checks `plan` against the active snapshot `old`. Returns every violation.
///
/// `owners` is engine configuration (`DSYNC_OWNER_MEMBER_IDS`): member ids
/// and/or `provider:login` entries. The first snapshot needs an owner in the
/// `provider:login` form.
pub fn authorize(
    old: Option<&Snapshot>,
    plan: &ChangePlan,
    ctx: &ChangeContext,
    owners: &[String],
) -> Result<(), Vec<Violation>> {
    let mut v = Vec::new();
    let viol = |code, item: &str, message: String| Violation { code, item: item.to_string(), message };

    // The plan must have been computed against this active snapshot.
    if old.map(|o| o.content_hash.clone()) != plan.old_hash {
        return Err(vec![viol(
            ViolationCode::PlanMismatch,
            "plan",
            "the plan was not computed against the active snapshot".into(),
        )]);
    }

    // Bootstrap: no active snapshot, so nobody can be resolved; only an
    // owner identity from engine configuration may load the first one.
    let Some(roster) = old else {
        if is_owner_identity(&ctx.actor, owners) {
            return Ok(());
        }
        return Err(vec![viol(
            ViolationCode::NotBootstrapOwner,
            "bootstrap",
            format!("{} is not an owner; only an owner may load the first snapshot", ctx.actor),
        )]);
    };

    // The actor, resolved through the ACTIVE snapshot only.
    let Some(actor) = resolve(roster, &ctx.actor, owners, ctx.today) else {
        return Err(vec![viol(
            ViolationCode::UnknownActor,
            "actor",
            format!("{} is not a member of the active snapshot", ctx.actor),
        )]);
    };
    if !actor.active {
        return Err(vec![viol(
            ViolationCode::ActorNotActive,
            "actor",
            format!("{} is not an active member", ctx.actor),
        )]);
    }

    let author = ctx.author.as_ref().map(|a| resolve(roster, a, owners, ctx.today).unwrap_or(Principal {
        identity: a.clone(),
        member_id: None,
        owner: false,
        active: false,
        rank: 0,
        permissions: BTreeSet::new(),
    }));
    let approvers: Vec<(ForgeIdentity, Option<Principal>)> = ctx
        .approvers
        .iter()
        .map(|a| (a.clone(), resolve(roster, a, owners, ctx.today)))
        .collect();

    for req in requirements(plan) {
        let mut rank = req.rank;
        if let Some(t) = &req.target {
            if target_is_owner(t, owners) {
                rank = OWNER_RANK;
            }
        }
        let item = req.item.as_str();

        // Nobody changes their own grants, status, identities or entry.
        if let Some(t) = &req.target {
            if actor.is(t) {
                v.push(viol(ViolationCode::SelfChange, item, format!("{} may not change their own record", ctx.actor)));
            }
        }
        if !outranks(actor.rank, rank) {
            v.push(viol(
                ViolationCode::InsufficientRank,
                item,
                format!("{} has rank {}; this change needs rank above {rank}", ctx.actor, actor.rank),
            ));
        }
        if !actor.has(req.permission) {
            v.push(viol(
                ViolationCode::MissingPermission,
                item,
                format!("{} lacks `{}`", ctx.actor, req.permission),
            ));
        }

        if !req.two_person {
            continue;
        }
        if !ctx.via_pull_request {
            v.push(viol(
                ViolationCode::DirectPushNeedsApproval,
                item,
                "a two-person change must arrive through a merged pull request with an approving review; a direct push carries no approver".into(),
            ));
            continue;
        }
        let Some(author) = &author else {
            v.push(viol(
                ViolationCode::PullRequestAuthorUnknown,
                item,
                "the pull request author is unknown, so no approver can be shown to be a second person".into(),
            ));
            continue;
        };

        let mut reasons = Vec::new();
        let mut ok = false;
        for (who, p) in &approvers {
            let Some(p) = p else {
                reasons.push(format!("{who}: not a member of the active snapshot"));
                continue;
            };
            if p.same_person(author) {
                reasons.push(format!("{who}: is the author"));
                continue;
            }
            if p.same_person(&actor) {
                reasons.push(format!("{who}: is the actor who merged"));
                continue;
            }
            if req.affected.iter().any(|t| p.is(t)) {
                reasons.push(format!("{who}: is affected by or benefits from this change"));
                continue;
            }
            if !p.active {
                reasons.push(format!("{who}: is not active"));
                continue;
            }
            if !outranks(p.rank, rank) {
                reasons.push(format!("{who}: rank {} is not above {rank}", p.rank));
                continue;
            }
            if !p.has(req.approver_permission) {
                reasons.push(format!("{who}: lacks `{}`", req.approver_permission));
                continue;
            }
            ok = true;
            break;
        }
        if !ok {
            if reasons.is_empty() {
                reasons.push("no approving review".into());
            }
            v.push(viol(ViolationCode::NoQualifiedApprover, item, reasons.join("; ")));
        }
    }

    if v.is_empty() {
        Ok(())
    } else {
        Err(v)
    }
}
