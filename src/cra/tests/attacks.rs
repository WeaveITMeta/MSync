//! Attack stories. Each test is one thing a real attacker would try against
//! the CRA repository, told as a story, with the control that stops it.

use super::*;
use ViolationCode as V;

fn old() -> Snapshot {
    load_ok(&base())
}

/// Story 1. Mallory has push access to the repository but is not a member.
/// In one commit she adds herself to `members.toml` with a Lead grant, and
/// while she is at it grants her friend cal a Reviewer seat. Her new entry
/// would make her rank 80, but the actor is resolved through the ACTIVE
/// snapshot, where she does not exist.
#[test]
fn story_outsider_adds_herself_with_a_high_grant_in_the_same_commit() {
    let mallory = "\n[[member]]\nid = \"mallory\"\nname = \"Mallory\"\nstatus = \"active\"\n[member.identities]\ngithub = \"mallory-gh\"\n[[member.grant]]\nrole = \"lead\"\ngranted_by = \"lena\"\nexpires = \"2027-06-01\"\n";
    let new = add_grant(append(base(), F::Members, mallory), "cal", "reviewer", "lena", None);
    // The new snapshot itself is valid: the files alone cannot tell.
    let r = auth(&old(), &new, &push("mallory-gh"));
    assert_eq!(vcodes(&r), vec![V::UnknownActor], "{r:?}");
    // Through a pull request she approves herself: still nobody.
    let r = auth(&old(), &new, &pr("mallory-gh", "mallory-gh", &["mallory-gh"]));
    assert_eq!(vcodes(&r), vec![V::UnknownActor], "{r:?}");
}

/// Story 1b. Cal is a member with no position. He edits his own entry to
/// give himself the Lead seat. He is known, but he may not touch his own
/// grants, and his rank (0) is nowhere near 80.
#[test]
fn story_member_grants_himself_a_high_seat() {
    let new = add_grant(base(), "cal", "lead", "lena", Some("2027-06-01"));
    let r = auth(&old(), &new, &push("cal-gh"));
    assert_denied(&r, V::SelfChange);
    assert_denied(&r, V::InsufficientRank);
    assert_denied(&r, V::MissingPermission);
    assert_denied(&r, V::DirectPushNeedsApproval);
}

/// Story 2. Mia is a Maintainer (60). She wants her friend cal to be a
/// Maintainer too, so she grants it and even gets a Lead to approve. A
/// Maintainer cannot create a peer: strict inequality, 60 is not above 60.
#[test]
fn story_maintainer_grants_maintainer_to_a_friend() {
    let new = add_grant(base(), "cal", "maintainer", "mia", Some("2027-06-01"));
    let r = auth(&old(), &new, &pr("mia-gh", "mia-gh", &["lena-gh"]));
    assert_eq!(vcodes(&r), vec![V::InsufficientRank], "{r:?}");
    // The same grant made by a Lead with the same approval is fine.
    let new = add_grant(base(), "cal", "maintainer", "lena", Some("2027-06-01"));
    assert_allowed(&auth(&old(), &new, &pr("lena-gh", "lena-gh", &["lars-gh"])));
}

/// Story 3. Rex and Ria are Reviewers (40). They agree to promote each other
/// to Maintainer (60), a rank above both. Each opens a pull request that
/// promotes the other, and each approves the other's pull request, then
/// merges their own.
#[test]
fn story_two_colluding_juniors_approve_each_others_promotion() {
    // Rex's pull request promotes Ria; Ria approves it; Rex merges.
    let promote_ria = add_grant(base(), "ria", "maintainer", "rex", Some("2027-06-01"));
    let r = auth(&old(), &promote_ria, &pr("rex-gh", "rex-gh", &["ria-gh"]));
    assert_denied(&r, V::InsufficientRank); // 40 is not above 60
    assert_denied(&r, V::NoQualifiedApprover); // Ria benefits and is not above 60
    // Ria's pull request promotes Rex; Rex approves it; Ria merges.
    let promote_rex = add_grant(base(), "rex", "maintainer", "ria", Some("2027-06-01"));
    let r = auth(&old(), &promote_rex, &pr("ria-gh", "ria-gh", &["rex-gh"]));
    assert_denied(&r, V::InsufficientRank);
    assert_denied(&r, V::NoQualifiedApprover);
    // One combined pull request promoting both, approved by both.
    let both = add_grant(promote_ria, "rex", "maintainer", "ria", Some("2027-06-01"));
    let r = auth(&old(), &both, &pr("rex-gh", "rex-gh", &["ria-gh", "rex-gh"]));
    assert_denied(&r, V::SelfChange);
    assert_denied(&r, V::InsufficientRank);
    assert_denied(&r, V::NoQualifiedApprover);
}

/// Story 4. Cal wants to act as Lena (a Lead) and collect her payouts. He
/// rewrites his own GitHub identity to Lena's login.
#[test]
fn story_attacker_rewrites_his_identity_to_a_senior_members_login() {
    // Taking Lena's login while she still holds it: the snapshot is invalid.
    let steal = edit(base(), F::Members, "github = \"cal-gh\"", "github = \"Lena-GH\"");
    assert!(error_codes(&steal).contains(&ErrorCode::DuplicateIdentity));

    // So he also moves Lena off her login in the same commit. Now the files
    // are valid, and authorization stops both edits.
    let swap = edit(steal, F::Members, "github = \"lena-gh\"", "github = \"lena-old\"");
    let r = auth(&old(), &swap, &push("cal-gh"));
    let v = r.clone().unwrap_err();
    assert!(v.iter().any(|x| x.code == V::SelfChange && x.item.contains("`cal` github")), "{v:?}");
    assert!(v.iter().any(|x| x.code == V::InsufficientRank && x.item.contains("`lena` github")), "{v:?}");

    // Even after pushing, the next commit cal makes as "lena-gh" is resolved
    // through the ACTIVE snapshot, where that login still belongs to Lena.
    // He does not have her login on GitHub, so he cannot act as her; and
    // his own login maps to cal, rank 0.
    let r = auth(&old(), &add_grant(base(), "cal", "reviewer", "lena", None), &push("cal-gh"));
    assert_denied(&r, V::InsufficientRank);
}

/// Story 5. A Director of a closed company switches the organization from
/// closed to open by pushing straight to the default branch, which would
/// expose the roster publicly. A mode switch is two-person; a push carries
/// no approver. Uses the Weave Solutions example.
#[test]
fn story_direct_push_switches_mode_from_closed_to_open() {
    let closed = example("weave-solutions");
    let old = load_ok(&closed);
    let open = edit(closed, F::Org, "mode = \"closed\"", "mode = \"open\"");
    // Weave declares a compartment, which open mode disables by default.
    let open = edit(open, F::Org, "[policy]", "[policy]\n\"compartments.enabled\" = true");
    let new = load_ok(&open);
    let plan = plan_change(Some(&old), &new);
    let m = plan.mode_change.as_ref().expect("mode change detected");
    assert_eq!((m.old, m.new), (Some(Mode::Closed), Mode::Open));
    assert!(plan.policy_changes.iter().any(|c| c.path == "visibility.roster"));

    let weave_owners = vec!["github:example-founder".to_string()];
    let ctx = ChangeContext { actor: gh("Simbuilder"), via_pull_request: false, author: None, approvers: vec![], today: today() };
    let r = authorize(Some(&old), &plan, &ctx, &weave_owners);
    assert_denied(&r, V::DirectPushNeedsApproval);
    let items: Vec<String> = r.unwrap_err().into_iter().map(|v| v.item).collect();
    assert!(items.iter().any(|i| i == "mode closed to open"), "{items:?}");

    // The preview shows the switch and its impact before anything is merged.
    let md = plan.render_markdown();
    println!("{md}");
    assert!(md.contains("`closed` to `open`"), "{md}");
    assert!(md.contains("visibility.roster"), "{md}");

    // Through a pull request approved by the owner, it is authorized.
    let ctx = ChangeContext {
        actor: gh("Simbuilder"),
        via_pull_request: true,
        author: Some(gh("Simbuilder")),
        approvers: vec![gh("example-founder")],
        today: today(),
    };
    assert_allowed(&authorize(Some(&old), &plan, &ctx, &weave_owners));
}
