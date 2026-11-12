//! The three example CRAs under `examples/cra/` load, validate, bootstrap and
//! project as documented.

use std::collections::BTreeMap;

use super::*;
use crate::cra::model::{ForgeProvider, RepoVisibility};
use crate::cra::policy::PayoutEligible;

const SLUGS: &[&str] = &["eustress-engine", "weave-solutions", "voltec-industries"];

fn founder() -> Vec<String> {
    vec!["github:example-founder".to_string()]
}

#[test]
fn every_example_loads_validates_and_bootstraps_by_its_owner() {
    for slug in SLUGS {
        let files = example(slug);
        let s = match load(&files, today()) {
            Ok(s) => s,
            Err(e) => panic!("{slug}: {}", e.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("\n")),
        };
        assert_eq!(&s.org.org.slug, slug);
        let plan = plan_change(None, &s);
        let ctx = |who: &str| ChangeContext { actor: gh(who), via_pull_request: false, author: None, approvers: vec![], today: today() };
        assert!(authorize(None, &plan, &ctx("example-founder"), &founder()).is_ok(), "{slug}");
        assert!(authorize(None, &plan, &ctx("Simbuilder"), &founder()).is_err(), "{slug}: a non-owner bootstrapped");
        let md = plan.render_markdown();
        assert!(md.contains("first snapshot"), "{slug}: {md}");
    }
}

#[test]
fn examples_have_the_modes_they_claim() {
    let mode = |slug: &str| load_ok(&example(slug)).mode_label;
    assert_eq!(mode("eustress-engine"), Mode::Open);
    assert_eq!(mode("weave-solutions"), Mode::Closed);
    assert_eq!(mode("voltec-industries"), Mode::Hybrid);
}

#[test]
fn mckale_has_a_different_standing_in_each_organization() {
    let st = |slug: &str| standing(&load_ok(&example(slug)), "mckale", 0, today()).unwrap();
    let e = st("eustress-engine");
    let w = st("weave-solutions");
    let v = st("voltec-industries");
    assert_eq!((e.rank, e.titles[0].as_str()), (80, "Lead Maintainer"));
    assert_eq!((w.rank, w.titles[0].as_str()), (70, "Director"));
    assert_eq!((v.rank, v.titles[0].as_str()), (20, "Engineer"));
    assert!(e.permissions.contains("policy.approve_change"));
    assert!(w.permissions.contains("payouts.approve"));
    assert!(!v.permissions.contains("members.grant"));
    // The same GitHub login resolves to each org's own member.
    for slug in SLUGS {
        let s = load_ok(&example(slug));
        assert_eq!(s.member_by_forge(&gh("simbuilder")).map(|m| m.id.as_str()), Some("mckale"));
    }
}

#[test]
fn only_mckale_is_a_real_person_and_every_discord_id_is_a_fake_snowflake() {
    for slug in SLUGS {
        let s = load_ok(&example(slug));
        for m in &s.members.members {
            if m.id != "mckale" {
                assert!(m.name.contains("Example"), "{slug}: {} is not an obvious placeholder", m.name);
            }
            if let Some(d) = &m.identities.discord {
                assert!(d.starts_with(['1', '2', '3']) && d[1..].starts_with("000000000000"), "{slug}: {d}");
            }
        }
        for r in &s.roles.roles {
            if let Some(d) = &r.discord_role {
                assert!(d[1..].starts_with("000000000000"), "{slug}: {d}");
            }
        }
        for f in &s.org.forges {
            assert!(f.repo.starts_with("example-"), "{slug}: {} is not a placeholder repository", f.repo);
        }
    }
}

#[test]
fn eustress_is_open_source_with_earned_tiers_and_appointed_positions() {
    let s = load_ok(&example("eustress-engine"));
    assert_eq!(s.org.forges.len(), 1);
    assert_eq!(s.org.forges[0].visibility, RepoVisibility::Public);
    assert_eq!(s.org.consumers.len(), 1);
    assert_eq!(s.payouts.schedule.cadence, Cadence::Weekly);
    let positions: Vec<&str> = s.roles.roles.iter().filter(|r| r.kind == RoleKind::Position).map(|r| r.title.as_str()).collect();
    assert_eq!(positions, vec!["Reviewer", "Maintainer", "Lead Maintainer"]);
    // A contributor with 600 merit is a Core Contributor at rank 30, the
    // ceiling, with no authority permission.
    let cy = standing(&s, "cy-example", 600, today()).unwrap();
    assert_eq!((cy.rank, cy.tier.as_ref().unwrap().title.as_str()), (30, "Core Contributor"));
    assert!(cy.permissions.iter().all(|p| s.permission_class(p) != Some(PermissionClass::Authority)));
    // Discord roles are ordered by rank so the titles form the hierarchy.
    let proj = discord_projection(&s, &BTreeMap::from([("cy-example".to_string(), 600)]), today());
    let order: Vec<&str> = proj.role_order.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(order, vec!["Lead Maintainer", "Maintainer", "Reviewer", "Core Contributor", "Active Contributor", "Contributor"]);
    // mckale holds Lead Maintainer; cy has no Discord identity, so nothing.
    assert!(proj.assignments.contains(&("100000000000000102".to_string(), "100000000000000080".to_string())));
    assert!(!proj.assignments.iter().any(|(_, r)| r == "100000000000000030"));
}

#[test]
fn weave_merit_is_recorded_but_confers_no_rank() {
    let s = load_ok(&example("weave-solutions"));
    assert_eq!(s.policy.promotion_merit_rank_ceiling, 0);
    assert_eq!(s.policy.payouts_eligible, PayoutEligible::PositionsOnly);
    assert!(s.org.forges.iter().all(|f| f.visibility == RepoVisibility::Private));
    assert!(s.roles.roles.iter().filter(|r| r.kind == RoleKind::Tier).all(|r| r.rank == 0));
    assert_eq!(s.payouts.schedule.cadence, Cadence::Monthly);
    assert!(s.payouts.eligibility.roles.iter().all(|k| s.role(k).unwrap().kind == RoleKind::Position));
    // An engineer with a huge merit balance still ranks as an Engineer.
    let eli = standing(&s, "eli-example", 1_000_000, today()).unwrap();
    assert_eq!(eli.rank, 20);
    assert!(s.merit.events.iter().all(|e| !e.external_counts));
}

#[test]
fn voltec_exercises_both_forges_and_keeps_its_portal_on_community_scopes() {
    let s = load_ok(&example("voltec-industries"));
    let providers: Vec<ForgeProvider> = s.org.forges.iter().map(|f| f.provider).collect();
    assert_eq!(providers, vec![ForgeProvider::Github, ForgeProvider::Gitlab]);
    assert!(s.roles.roles.iter().any(|r| r.title == "Head of Cell Engineering" && r.compartments == vec!["cell-eng".to_string()]));
    assert!(s.org.compartments.iter().any(|c| c.key == "cell-eng"));
    assert_eq!(s.org.consumers.len(), 1);
    // Voltec's rule: DSync standing never reaches beyond community.*.
    for c in &s.org.consumers {
        assert!(c.scopes.iter().all(|x| x.starts_with("community.")), "{:?}", c.scopes);
    }
    // A GitLab-only member resolves through the GitLab identity.
    let hana = s.member_by_forge(&ForgeIdentity::gitlab("HANA-example")).unwrap();
    assert_eq!(hana.id, "hana-example");
    assert!(s.member_by_forge(&gh("hana-example")).is_none(), "providers must not cross");
}

#[test]
fn voltec_renewing_a_two_person_grant_needs_a_second_person() {
    let files = example("voltec-industries");
    let old = load_ok(&files);
    let renewed = edit(files, F::Members, "expires = \"2027-05-01\"", "expires = \"2027-05-10\"");
    let new = load_ok(&renewed);
    let plan = plan_change(Some(&old), &new);
    assert_eq!(plan.grants_changed.len(), 1);
    let ctx = |via_pr: bool, approvers: &[&str]| ChangeContext {
        actor: gh("gus-example"),
        via_pull_request: via_pr,
        author: via_pr.then(|| gh("gus-example")),
        approvers: approvers.iter().map(|a| gh(a)).collect(),
        today: today(),
    };
    let r = authorize(Some(&old), &plan, &ctx(false, &[]), &founder());
    assert!(vcodes(&r).contains(&ViolationCode::DirectPushNeedsApproval), "{r:?}");
    // Hana cannot approve her own renewal (and she is not above 60 anyway).
    let hana = ChangeContext { approvers: vec![ForgeIdentity::gitlab("hana-example")], ..ctx(true, &[]) };
    let r = authorize(Some(&old), &plan, &hana, &founder());
    assert!(vcodes(&r).contains(&ViolationCode::NoQualifiedApprover), "{r:?}");
    assert!(authorize(Some(&old), &plan, &ctx(true, &["example-founder"]), &founder()).is_ok());
}

#[test]
fn voltec_head_of_cell_engineering_may_staff_her_team_but_not_above_herself() {
    let files = example("voltec-industries");
    let old = load_ok(&files);
    // Hana (60) grants Engineer (20) to a new GitLab-only member, by push.
    let newbie = "\n[[member]]\nid = \"jo-example\"\nname = \"Jo Example\"\nstatus = \"active\"\n\n[member.identities]\ngitlab = \"jo-example\"\n\n[[member.grant]]\nrole = \"engineer\"\ngranted_by = \"hana-example\"\n";
    let new = load_ok(&append(files.clone(), F::Members, newbie));
    let plan = plan_change(Some(&old), &new);
    let ctx = ChangeContext { actor: ForgeIdentity::gitlab("hana-example"), via_pull_request: false, author: None, approvers: vec![], today: today() };
    assert!(authorize(Some(&old), &plan, &ctx, &founder()).is_ok(), "{:?}", authorize(Some(&old), &plan, &ctx, &founder()));
    // She cannot appoint a Director (70).
    let director = newbie.replace("role = \"engineer\"", "role = \"director\"\nexpires = \"2027-01-01\"");
    let new = load_ok(&append(files, F::Members, &director));
    let plan = plan_change(Some(&old), &new);
    let ctx = ChangeContext { via_pull_request: true, author: Some(ForgeIdentity::gitlab("hana-example")), approvers: vec![gh("example-founder")], ..ctx };
    let r = authorize(Some(&old), &plan, &ctx, &founder());
    assert!(vcodes(&r).contains(&ViolationCode::InsufficientRank), "{r:?}");
}
