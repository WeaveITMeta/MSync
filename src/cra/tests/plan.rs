use super::*;

fn old() -> Snapshot {
    load_ok(&base())
}

#[test]
fn plan_lists_every_kind_of_change() {
    let mut files = add_grant(base(), "cal", "reviewer", "mia", None);
    files = edit(files, F::Members, "[[member.grant]]\nrole = \"reviewer\"\ngranted_by = \"mia\"\n# end:ria", "# end:ria");
    files = edit(files, F::Members, "id = \"rex\"\nname = \"Rex Reviewer\"\nstatus = \"active\"", "id = \"rex\"\nname = \"Rex Reviewer\"\nstatus = \"suspended\"");
    files = edit(files, F::Members, "github = \"max-gh\"", "github = \"max-new\"");
    files = edit(files, F::Members, "expires = \"2027-06-01\"\n# end:mia", "expires = \"2027-05-01\"\n# end:mia");
    files = append(files, F::Members, "\n[[member]]\nid = \"neo\"\nname = \"Neo\"\nstatus = \"active\"\n");
    files = append(files, F::Roles, "\n[[role]]\nkey = \"helper\"\ntitle = \"Helper\"\nkind = \"position\"\nrank = 10\n");
    files = edit(files, F::Roles, "title = \"Reviewer\"", "title = \"Senior Reviewer\"");
    files = edit(files, F::Merit, "daily_cap = 150", "daily_cap = 200");
    files = edit(files, F::Payouts, "base_cents = 1000", "base_cents = 1200");
    files = edit(files, F::Org, "name = \"Test Org\"", "name = \"Test Org Renamed\"");
    files = edit(files, F::Org, "[discord]", "[policy]\n\"payouts.hold_days\" = 9\n\n[discord]");
    files = edit(files, F::Payouts, "hold_days = 7", "hold_days = 9");
    let new = load_ok(&files);
    let p = plan_change(Some(&old()), &new);

    assert_eq!(p.grants_added.iter().map(|g| (g.member.id.as_str(), g.role.as_str())).collect::<Vec<_>>(), vec![("cal", "reviewer")]);
    assert_eq!(p.grants_removed.iter().map(|g| (g.member.id.as_str(), g.role.as_str())).collect::<Vec<_>>(), vec![("ria", "reviewer")]);
    assert_eq!(p.grants_changed.len(), 1);
    assert_eq!(p.grants_changed[0].member.id, "mia");
    assert_eq!(p.status_changes.len(), 1);
    assert_eq!(p.status_changes[0].new, MemberStatus::Suspended);
    assert_eq!(p.identity_changes.len(), 1);
    assert_eq!((p.identity_changes[0].member.id.as_str(), p.identity_changes[0].field.as_str()), ("max", "github"));
    assert_eq!(p.members_added.len(), 1);
    assert_eq!(p.roles_added.len(), 1);
    assert_eq!(p.roles_changed.len(), 1);
    assert_eq!(p.roles_changed[0].changes[0].path, "title");
    assert_eq!(p.merit_changes.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(), vec!["event[pr_merged].daily_cap"]);
    assert!(p.payout_changes.iter().any(|c| c.path == "rate[main].base_cents"));
    assert!(p.payout_changes.iter().any(|c| c.path == "limits.hold_days"));
    assert!(p.org_changes.iter().any(|c| c.path == "org.name"));
    assert_eq!(p.policy_changes.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(), vec!["payouts.hold_days"]);
    assert!(p.mode_change.is_none());

    // Impact: cal gains the Reviewer Discord role; ria and rex lose it.
    assert_eq!(p.impact.discord_role_gains.get("900000000000000040"), Some(&vec!["cal".to_string()]));
    assert_eq!(p.impact.discord_role_losses.get("900000000000000040"), Some(&vec!["rex".to_string(), "ria".to_string()]));

    let md = p.render_markdown();
    for needle in ["### Policy", "### Organization", "### Roles", "### Members", "### Grants", "### Merit", "### Payouts", "### Impact", "### Authority needed", "`max` github"] {
        assert!(md.contains(needle), "missing {needle} in\n{md}");
    }
    assert!(!md.contains('\u{2014}'), "no em dashes in the preview");
}

#[test]
fn reordering_entries_is_not_a_change() {
    // Swap the two tier blocks: identical content, different order.
    let roles = ROLES.to_string();
    let a = roles.find("[[role]]\nkey = \"newcomer\"").unwrap();
    let b = roles.find("[[role]]\nkey = \"contributor\"").unwrap();
    let c = roles.find("[[role]]\nkey = \"reviewer\"").unwrap();
    let swapped = format!("{}{}{}{}", &roles[..a], &roles[b..c], &roles[a..b], &roles[c..]);
    let files = CraFiles { roles: swapped, ..base() };
    let p = plan_change(Some(&old()), &load_ok(&files));
    assert!(p.is_empty(), "{p:?}");
    assert_ne!(p.old_hash.as_deref(), Some(p.new_hash.as_str()), "the bytes did change");
    assert!(p.render_markdown().contains("No changes."));
}

#[test]
fn mode_switch_preview_shows_policy_diff_and_tiers_over_the_new_ceiling() {
    // Open to closed while leaving the tier at rank 10: the new snapshot is
    // invalid, but the preview is still computed from a parsed snapshot so the
    // author sees what to fix.
    let files = edit(base(), F::Org, "mode = \"open\"", "mode = \"closed\"");
    let new = Snapshot::parse(&files).unwrap();
    assert!(new.validate(today()).is_err());
    let p = plan_change(Some(&old()), &new);
    let m = p.mode_change.as_ref().unwrap();
    assert_eq!((m.old, m.new), (Some(Mode::Open), Mode::Closed));
    assert_eq!(p.impact.new_merit_rank_ceiling, 0);
    assert_eq!(p.impact.tiers_exceeding_new_ceiling, vec![("contributor".to_string(), 10)]);
    let fields: Vec<&str> = p.policy_changes.iter().map(|c| c.path.as_str()).collect();
    for f in ["visibility.roster", "promotion.merit_rank_ceiling", "grants.two_person_min_rank", "payouts.hold_days"] {
        assert!(fields.contains(&f), "{f} missing from {fields:?}");
    }
    // Thresholds: two-person at the lower (30), org changes above the higher (60).
    assert_eq!((p.two_person_min_rank, p.org_change_rank), (30, 60));
    let md = p.render_markdown();
    assert!(md.contains("`open` to `closed`"), "{md}");
    assert!(md.contains("`contributor` (rank 10)"), "{md}");
}

#[test]
fn bootstrap_plan_lists_everything_as_added() {
    let new = load_ok(&base());
    let p = plan_change(None, &new);
    assert_eq!(p.roles_added.len(), 5);
    assert_eq!(p.members_added.len(), 8);
    assert_eq!(p.mode_change.as_ref().unwrap().old, None);
    assert!(p.render_markdown().contains("first snapshot"));
}
