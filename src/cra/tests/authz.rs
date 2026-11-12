//! One test per authorization rule (contract section 4 step 4).
//!
//! Base fixture ranks: owner `test-owner` 1000 (config), lena and lars Lead
//! 80, mia and max Maintainer 60, rex and ria Reviewer 40, cal no position.
//! Open mode: two-person at rank 60 and above, merit ceiling 30.

use super::*;
use ViolationCode as V;

fn old() -> Snapshot {
    load_ok(&base())
}

// ------------------------------------------------------------- bootstrap

#[test]
fn bootstrap_only_an_owner_may_load_the_first_snapshot() {
    let new = load_ok(&base());
    let plan = plan_change(None, &new);
    assert!(plan.old_hash.is_none());
    assert_allowed(&authorize(None, &plan, &push("test-owner"), &owners()));
    // Owner logins are case-insensitive.
    assert_allowed(&authorize(None, &plan, &push("TEST-OWNER"), &owners()));
    // A Lead in the NEW snapshot is still nobody before a snapshot exists.
    let r = authorize(None, &plan, &push("lena-gh"), &owners());
    assert_denied(&r, V::NotBootstrapOwner);
    // Even with an approving review.
    let r = authorize(None, &plan, &pr("lena-gh", "lena-gh", &["test-owner"]), &owners());
    assert_denied(&r, V::NotBootstrapOwner);
}

#[test]
fn bootstrap_cannot_use_a_member_id_owner_because_the_new_snapshot_decides_ids() {
    // Owner configured as member id "root". The new snapshot maps "root" to
    // github "test-owner", but the new snapshot cannot vouch for itself.
    let new = load_ok(&base());
    let plan = plan_change(None, &new);
    let r = authorize(None, &plan, &push("test-owner"), &["root".to_string()]);
    assert_denied(&r, V::NotBootstrapOwner);
}

#[test]
fn a_plan_computed_against_another_snapshot_is_refused() {
    let o = old();
    let other = load_ok(&append(base(), F::Org, "\n"));
    let new = load_ok(&add_grant(base(), "cal", "reviewer", "mia", None));
    let plan = plan_change(Some(&other), &new);
    assert_denied(&authorize(Some(&o), &plan, &push("lena-gh"), &owners()), V::PlanMismatch);
    let plan = plan_change(None, &new);
    assert_denied(&authorize(Some(&o), &plan, &push("test-owner"), &owners()), V::PlanMismatch);
}

// ------------------------------------------------------------- the actor

#[test]
fn an_unknown_actor_is_refused() {
    let r = auth(&old(), &add_grant(base(), "cal", "reviewer", "mia", None), &push("stranger"));
    assert_denied(&r, V::UnknownActor);
}

#[test]
fn a_suspended_actor_is_refused() {
    let suspended = edit(base(), F::Members, "id = \"mia\"
name = \"Mia Maintainer\"
status = \"active\"", "id = \"mia\"
name = \"Mia Maintainer\"
status = \"suspended\"");
    let o = load_ok(&suspended);
    let new = add_grant(suspended, "cal", "reviewer", "mia", None);
    assert_denied(&auth(&o, &new, &push("mia-gh")), V::ActorNotActive);
}

#[test]
fn an_expired_grant_confers_no_authority() {
    // mia's Maintainer grant expires 2027-06-01. On 2027-06-02 she has no rank.
    let new = add_grant(base(), "cal", "reviewer", "mia", None);
    let mut ctx = push("mia-gh");
    assert_allowed(&auth(&old(), &new, &ctx));
    ctx.today = chrono::NaiveDate::from_ymd_opt(2027, 6, 2).unwrap();
    let r = auth(&old(), &new, &ctx);
    assert_denied(&r, V::InsufficientRank);
    assert_denied(&r, V::MissingPermission);
}

// --------------------------------------------------------- grants and rank

#[test]
fn a_grant_below_the_actor_and_below_threshold_is_a_one_person_change() {
    // Maintainer (60) grants Reviewer (40) by direct push.
    assert_allowed(&auth(&old(), &add_grant(base(), "cal", "reviewer", "mia", None), &push("mia-gh")));
}

#[test]
fn granting_at_equal_rank_is_refused() {
    // Reviewer (40) grants Reviewer (40).
    let r = auth(&old(), &add_grant(base(), "cal", "reviewer", "rex", None), &push("rex-gh"));
    assert_denied(&r, V::InsufficientRank);
}

#[test]
fn granting_above_own_rank_is_refused() {
    let r = auth(&old(), &add_grant(base(), "cal", "lead", "mia", Some("2027-06-01")), &pr("mia-gh", "mia-gh", &["test-owner"]));
    assert_denied(&r, V::InsufficientRank);
}

#[test]
fn granting_needs_the_grant_permission() {
    // Strip members.grant from Reviewer; rex (40) then grants a rank-10 position.
    let files = append(base(), F::Roles, "\n[[role]]\nkey = \"helper\"\ntitle = \"Helper\"\nkind = \"position\"\nrank = 10\n");
    let o = load_ok(&edit(files.clone(), F::Roles, "permissions = [\"members.grant\", \"members.approve_grant\", \"community.moderate\"]", "permissions = [\"community.moderate\"]"));
    let new = add_grant(edit(files, F::Roles, "permissions = [\"members.grant\", \"members.approve_grant\", \"community.moderate\"]", "permissions = [\"community.moderate\"]"), "cal", "helper", "rex", None);
    let r = auth(&o, &new, &push("rex-gh"));
    assert_denied(&r, V::MissingPermission);
}

#[test]
fn nobody_changes_their_own_grants() {
    // A Lead (80) grants herself Maintainer (60): rank and approver are fine,
    // only the self rule stops it.
    let new = add_grant(base(), "lena", "maintainer", "lars", Some("2027-06-01"));
    let r = auth(&old(), &new, &pr("lena-gh", "lena-gh", &["lars-gh"]));
    assert_eq!(vcodes(&r), vec![V::SelfChange], "{r:?}");
}

#[test]
fn nobody_removes_their_own_grant_either() {
    let new = edit(base(), F::Members, "[[member.grant]]\nrole = \"reviewer\"\ngranted_by = \"mia\"\n# end:rex", "# end:rex");
    let r = auth(&old(), &new, &push("rex-gh"));
    assert_denied(&r, V::SelfChange);
}

#[test]
fn revoking_needs_rank_above_the_role_and_the_member() {
    // mia (60) may revoke rex's Reviewer (40).
    let new = edit(base(), F::Members, "[[member.grant]]\nrole = \"reviewer\"\ngranted_by = \"mia\"\n# end:rex", "# end:rex");
    assert_allowed(&auth(&old(), &new, &push("mia-gh")));
    // Give lena a Reviewer grant too; mia (60) may not revoke it from a Lead (80).
    let with = add_grant(base(), "lena", "reviewer", "mia", None);
    let o = load_ok(&with);
    let r = auth(&o, &base(), &pr("mia-gh", "mia-gh", &["lars-gh"]));
    assert_denied(&r, V::InsufficientRank);
}

#[test]
fn revoking_needs_the_revoke_permission() {
    // rex (40) has members.grant but not members.revoke; cal holds a rank-10 helper.
    let files = append(base(), F::Roles, "\n[[role]]\nkey = \"helper\"\ntitle = \"Helper\"\nkind = \"position\"\nrank = 10\n");
    let o = load_ok(&add_grant(files.clone(), "cal", "helper", "rex", None));
    let r = auth(&o, &files, &push("rex-gh"));
    assert_denied(&r, V::MissingPermission);
}

// ------------------------------------------------------------- status

#[test]
fn changing_status_needs_rank_above_the_member() {
    let suspend = |files: CraFiles, id: &str, name: &str| {
        edit(files, F::Members, &format!("id = \"{id}\"\nname = \"{name}\"\nstatus = \"active\""), &format!("id = \"{id}\"\nname = \"{name}\"\nstatus = \"suspended\""))
    };
    // mia (60) suspends rex (40).
    assert_allowed(&auth(&old(), &suspend(base(), "rex", "Rex Reviewer"), &push("mia-gh")));
    // mia (60) may not suspend max (60): equal rank.
    let r = auth(&old(), &suspend(base(), "max", "Max Maintainer"), &pr("mia-gh", "mia-gh", &["lars-gh"]));
    assert_denied(&r, V::InsufficientRank);
    // Nobody changes their own status.
    let r = auth(&old(), &suspend(base(), "lena", "Lena Lead"), &pr("lena-gh", "lena-gh", &["test-owner"]));
    assert_denied(&r, V::SelfChange);
    // A member with no grants is still protected up to the merit ceiling (30):
    // cal holds only a rank-20 helper seat and may not suspend neo, who has
    // no grant at all, because neo may have earned a tier up to rank 30.
    let files = append(base(), F::Roles, "\n[[role]]\nkey = \"helper\"\ntitle = \"Helper\"\nkind = \"position\"\nrank = 20\npermissions = [\"members.suspend\"]\n");
    let files = add_grant(files, "cal", "helper", "mia", None);
    let files = append(files, F::Members, "\n[[member]]\nid = \"neo\"\nname = \"Neo\"\nstatus = \"active\"\n");
    let o = load_ok(&files);
    let r = auth(&o, &suspend(files, "neo", "Neo"), &push("cal-gh"));
    assert_eq!(vcodes(&r), vec![V::InsufficientRank], "{r:?}");
}

#[test]
fn suspending_a_two_person_rank_member_needs_two_people() {
    // lena (80) suspends mia (60): rank fine, but mia is at the threshold.
    let new = edit(base(), F::Members, "id = \"mia\"\nname = \"Mia Maintainer\"\nstatus = \"active\"", "id = \"mia\"\nname = \"Mia Maintainer\"\nstatus = \"suspended\"");
    assert_denied(&auth(&old(), &new, &push("lena-gh")), V::DirectPushNeedsApproval);
    assert_allowed(&auth(&old(), &new, &pr("lena-gh", "lena-gh", &["lars-gh"])));
}

// ------------------------------------------------------------- roles

#[test]
fn changing_a_role_needs_rank_above_old_and_new_rank() {
    // lena (80) lowers Reviewer to 30: fine, single person (40 < 60).
    let new = edit(base(), F::Roles, "rank = 40", "rank = 30");
    let ctx_lena = push("lena-gh");
    assert_allowed(&auth(&old(), &new, &ctx_lena));
    // lena (80) raises an unheld rank-10 role to 90: needs rank above 90.
    let helper = append(base(), F::Roles, "\n[[role]]\nkey = \"helper\"\ntitle = \"Helper\"\nkind = \"position\"\nrank = 10\n");
    let new = append(base(), F::Roles, "\n[[role]]\nkey = \"helper\"\ntitle = \"Helper\"\nkind = \"position\"\nrank = 90\n");
    let r = auth(&load_ok(&helper), &new, &pr("lena-gh", "lena-gh", &["test-owner"]));
    assert_eq!(vcodes(&r), vec![V::InsufficientRank], "{r:?}");
    // Deleting the Lead role (80) needs above 80, even for a Lead.
    let o = old();
    let without_lead = edit(
        edit(edit(base(), F::Roles, "[[role]]\nkey = \"lead\"", "[[role]]\nkey = \"retired\""), F::Members, "[[member.grant]]\nrole = \"lead\"\ngranted_by = \"root\"\nexpires = \"2027-06-01\"\n# end:lena", "# end:lena"),
        F::Members,
        "[[member.grant]]\nrole = \"lead\"\ngranted_by = \"root\"\nexpires = \"2027-06-01\"\n# end:lars",
        "# end:lars",
    );
    let r = auth(&o, &without_lead, &pr("lena-gh", "lena-gh", &["lars-gh"]));
    assert_denied(&r, V::InsufficientRank);
}

#[test]
fn creating_a_role_needs_policy_propose_permission() {
    // mia (60) lacks policy.propose_change.
    let new = append(base(), F::Roles, "\n[[role]]\nkey = \"helper\"\ntitle = \"Helper\"\nkind = \"position\"\nrank = 10\n");
    assert_denied(&auth(&old(), &new, &push("mia-gh")), V::MissingPermission);
    assert_allowed(&auth(&old(), &new, &push("lena-gh")));
}

#[test]
fn a_role_holder_cannot_approve_a_change_to_their_own_role() {
    // Adding a permission to Maintainer (60) is two-person. max holds it.
    let new = edit(base(), F::Roles, "\"members.revoke\", \"members.suspend\"]\ndiscord_role = \"900000000000000060\"", "\"members.revoke\", \"members.suspend\", \"roster.read_full\"]\ndiscord_role = \"900000000000000060\"");
    let r = auth(&old(), &new, &pr("lena-gh", "lena-gh", &["max-gh"]));
    assert_denied(&r, V::NoQualifiedApprover);
    assert_allowed(&auth(&old(), &new, &pr("lena-gh", "lena-gh", &["lars-gh"])));
}

// ---------------------------------------------------------- two-person

fn promote_cal() -> CraFiles {
    add_grant(base(), "cal", "maintainer", "lena", Some("2027-06-01"))
}

#[test]
fn a_two_person_grant_by_direct_push_is_refused() {
    let r = auth(&old(), &promote_cal(), &push("lena-gh"));
    assert_denied(&r, V::DirectPushNeedsApproval);
}

#[test]
fn a_direct_push_cannot_claim_approvers() {
    // Approvers listed on a direct push are ignored: a push carries none.
    let mut ctx = pr("lena-gh", "lena-gh", &["lars-gh"]);
    ctx.via_pull_request = false;
    assert_denied(&auth(&old(), &promote_cal(), &ctx), V::DirectPushNeedsApproval);
}

#[test]
fn a_two_person_grant_with_a_qualified_approver_is_allowed() {
    assert_allowed(&auth(&old(), &promote_cal(), &pr("lena-gh", "lena-gh", &["lars-gh"])));
    // The owner can be the second person.
    assert_allowed(&auth(&old(), &promote_cal(), &pr("lena-gh", "lena-gh", &["test-owner"])));
    // Author and merger may differ; the approver is a third person.
    assert_allowed(&auth(&old(), &promote_cal(), &pr("lena-gh", "rex-gh", &["lars-gh"])));
}

#[test]
fn the_approver_may_not_be_the_author() {
    // lars authored, lena merged, lars approved his own pull request.
    let r = auth(&old(), &promote_cal(), &pr("lena-gh", "lars-gh", &["lars-gh"]));
    assert_eq!(vcodes(&r), vec![V::NoQualifiedApprover], "{r:?}");
}

#[test]
fn the_approver_may_not_be_the_actor() {
    // rex authored, lena approved and merged: one Lead decided alone.
    let r = auth(&old(), &promote_cal(), &pr("lena-gh", "rex-gh", &["lena-gh"]));
    assert_denied(&r, V::NoQualifiedApprover);
}

#[test]
fn the_approver_may_not_be_a_beneficiary() {
    // The beneficiary outranks the role, so only the beneficiary rule stops
    // it: lena grants lars (a Lead) Maintainer, and lars approves.
    let new = add_grant(base(), "lars", "maintainer", "lena", Some("2027-06-01"));
    let r = auth(&old(), &new, &pr("lena-gh", "lena-gh", &["lars-gh"]));
    assert_eq!(vcodes(&r), vec![V::NoQualifiedApprover], "{r:?}");
}

#[test]
fn the_approver_must_outrank_the_role_strictly() {
    // mia (60) approving a Maintainer (60) grant is not enough.
    let r = auth(&old(), &promote_cal(), &pr("lena-gh", "lena-gh", &["mia-gh"]));
    assert_denied(&r, V::NoQualifiedApprover);
}

#[test]
fn the_approver_needs_the_approval_permission() {
    // A dedicated rank-90 position without members.approve_grant.
    let files = append(base(), F::Roles, "\n[[role]]\nkey = \"auditor\"\ntitle = \"Auditor\"\nkind = \"position\"\nrank = 90\npermissions = [\"audit.read_all\"]\nterm_days = 30\n");
    let files = add_grant(files, "cal", "auditor", "root", Some("2026-12-01"));
    let o = load_ok(&files);
    let new = add_grant(files, "rex", "maintainer", "lena", Some("2027-06-01"));
    let r = auth(&o, &new, &pr("lena-gh", "lena-gh", &["cal-gh"]));
    assert_denied(&r, V::NoQualifiedApprover);
}

#[test]
fn an_approver_unknown_to_the_active_snapshot_does_not_count() {
    // The new snapshot adds "newbie"; their approval cannot count because
    // approvers resolve through the active snapshot only.
    let new = append(promote_cal(), F::Members, "\n[[member]]\nid = \"newbie\"\nname = \"New Bie\"\nstatus = \"active\"\n[member.identities]\ngithub = \"newbie-gh\"\n");
    let r = auth(&old(), &new, &pr("lena-gh", "lena-gh", &["newbie-gh"]));
    assert_denied(&r, V::NoQualifiedApprover);
}

#[test]
fn a_two_person_pull_request_needs_a_known_author() {
    let mut ctx = pr("lena-gh", "lena-gh", &["lars-gh"]);
    ctx.author = None;
    assert_denied(&auth(&old(), &promote_cal(), &ctx), V::PullRequestAuthorUnknown);
}

#[test]
fn raising_the_threshold_in_the_same_commit_does_not_escape_two_person() {
    let new = edit(promote_cal(), F::Org, "[discord]", "[policy]\n\"grants.two_person_min_rank\" = 90\n\n[discord]");
    let r = auth(&old(), &new, &push("lena-gh"));
    let denied_items: Vec<String> = r.clone().unwrap_err().into_iter().filter(|v| v.code == V::DirectPushNeedsApproval).map(|v| v.item).collect();
    assert!(denied_items.iter().any(|i| i.contains("grant `maintainer` to `cal`")), "{denied_items:?}");
}

// --------------------------------------------------- org-level changes

#[test]
fn merit_payout_and_policy_changes_are_always_two_person() {
    let changes = [
        edit(base(), F::Merit, "daily_cap = 150", "daily_cap = 300"),
        edit(base(), F::Payouts, "base_cents = 1000", "base_cents = 5000"),
        edit(base(), F::Org, "[discord]", "[policy]\n\"probation.days\" = 20\n\n[discord]"),
        edit(base(), F::Org, "payout_branches = [\"main\"]", "payout_branches = [\"main\", \"release\"]"),
    ];
    for new in changes {
        assert_denied(&auth(&old(), &new, &push("lena-gh")), V::DirectPushNeedsApproval);
        assert_allowed(&auth(&old(), &new, &pr("lena-gh", "lena-gh", &["lars-gh"])));
        // A Maintainer (60) is not above the org-change rank (60).
        assert_denied(&auth(&old(), &new, &pr("mia-gh", "mia-gh", &["lars-gh"])), V::InsufficientRank);
    }
}

// ---------------------------------------------------------- identities

#[test]
fn changing_identities_needs_authority_over_the_member() {
    // cal has no grant; protected up to the ceiling (30). mia (60) may relink.
    let new = edit(base(), F::Members, "github = \"cal-gh\"", "github = \"cal-new\"");
    assert_allowed(&auth(&old(), &new, &push("mia-gh")));
    // rex (40) holds members.grant and outranks 30: allowed too.
    assert_allowed(&auth(&old(), &new, &push("rex-gh")));
    // mia's own identity: self change and not above her own rank.
    let new = edit(base(), F::Members, "github = \"mia-gh\"", "github = \"mia-new\"");
    let r = auth(&old(), &new, &pr("mia-gh", "mia-gh", &["lena-gh"]));
    assert_denied(&r, V::SelfChange);
    assert_denied(&r, V::InsufficientRank);
}

#[test]
fn changing_a_two_person_rank_members_identity_needs_two_people() {
    // lena (80) changes mia's (60) Discord id: who gets paid is at stake.
    let new = edit(base(), F::Members, "discord = \"900000000000000103\"", "discord = \"900000000000000999\"");
    assert_denied(&auth(&old(), &new, &push("lena-gh")), V::DirectPushNeedsApproval);
    assert_allowed(&auth(&old(), &new, &pr("lena-gh", "lena-gh", &["lars-gh"])));
    // mia cannot approve the change to her own identity.
    assert_denied(&auth(&old(), &new, &pr("lena-gh", "lena-gh", &["mia-gh"])), V::NoQualifiedApprover);
}

#[test]
fn an_owners_member_entry_cannot_be_changed_from_the_repository() {
    // root is an owner (github:test-owner): rank 1000 for any change to it.
    let new = edit(base(), F::Members, "github = \"test-owner\"", "github = \"someone-else\"");
    let r = auth(&old(), &new, &pr("lena-gh", "lena-gh", &["lars-gh"]));
    assert_denied(&r, V::InsufficientRank);
}

#[test]
fn adding_and_removing_member_entries_need_authority() {
    let newbie = "\n[[member]]\nid = \"newbie\"\nname = \"New Bie\"\nstatus = \"active\"\n[member.identities]\ngithub = \"newbie-gh\"\n";
    // mia (60) has members.invite and outranks the ceiling (30).
    assert_allowed(&auth(&old(), &append(base(), F::Members, newbie), &push("mia-gh")));
    // rex (40) lacks members.invite.
    assert_denied(&auth(&old(), &append(base(), F::Members, newbie), &push("rex-gh")), V::MissingPermission);
    // Removing cal's entry needs members.suspend and rank above 30.
    let without_cal = edit(base(), F::Members, "[[member]]\nid = \"cal\"\nname = \"Cal Contributor\"\nstatus = \"active\"\n[member.identities]\ngithub = \"cal-gh\"\ndiscord = \"900000000000000201\"\n# end:cal\n", "");
    assert_allowed(&auth(&old(), &without_cal, &push("mia-gh")));
    assert_denied(&auth(&old(), &without_cal, &push("rex-gh")), V::MissingPermission);
}

#[test]
fn an_empty_change_by_a_member_is_allowed_and_by_a_stranger_is_not() {
    let o = old();
    let same = load_ok(&base());
    let plan = plan_change(Some(&o), &same);
    assert!(plan.is_empty());
    assert_allowed(&authorize(Some(&o), &plan, &push("cal-gh"), &owners()));
    assert_denied(&authorize(Some(&o), &plan, &push("stranger"), &owners()), V::UnknownActor);
}
