use std::collections::BTreeMap;

use chrono::NaiveDate;

use super::*;

#[test]
fn rank_is_the_highest_active_position_or_the_capped_tier() {
    let s = load_ok(&base());
    let lena = standing(&s, "lena", 0, today()).unwrap();
    assert_eq!(lena.rank, 80);
    assert_eq!(lena.titles, vec!["Lead Maintainer".to_string(), "Newcomer".to_string()]);
    let cal = standing(&s, "cal", 0, today()).unwrap();
    assert_eq!((cal.rank, cal.tier.unwrap().key.as_str()), (0, "newcomer"));
    let cal = standing(&s, "cal", 75, today()).unwrap();
    assert_eq!((cal.rank, cal.tier.as_ref().unwrap().key.as_str()), (10, "contributor"));
    assert!(cal.permissions.contains("community.post"));
    assert_eq!(cal.discord_roles, vec!["900000000000000010".to_string()]);
    assert!(standing(&s, "nobody", 0, today()).is_none());
}

#[test]
fn a_member_with_several_positions_takes_the_highest() {
    let s = load_ok(&add_grant(base(), "rex", "maintainer", "lena", Some("2027-01-01")));
    let rex = standing(&s, "rex", 0, today()).unwrap();
    assert_eq!(rex.rank, 60);
    assert_eq!(rex.positions.iter().map(|p| p.key.as_str()).collect::<Vec<_>>(), vec!["maintainer", "reviewer"]);
    assert!(rex.permissions.contains("members.revoke") && rex.permissions.contains("community.moderate"));
}

#[test]
fn expired_grants_do_not_count() {
    let s = load_ok(&base());
    let on_last_day = NaiveDate::from_ymd_opt(2027, 6, 1).unwrap();
    let after = NaiveDate::from_ymd_opt(2027, 6, 2).unwrap();
    assert_eq!(standing(&s, "mia", 0, on_last_day).unwrap().rank, 60);
    let mia = standing(&s, "mia", 0, after).unwrap();
    assert_eq!(mia.rank, 0);
    assert!(mia.positions.is_empty());
    assert!(!mia.permissions.contains("members.grant"));
    assert!(!mia.discord_roles.contains(&"900000000000000060".to_string()));
}

#[test]
fn the_tier_rank_is_capped_by_the_ceiling() {
    // Ceiling lowered to 5 by override, tier at 10 would be refused by
    // validation; parse only, to prove the read-time cap.
    let files = edit(base(), F::Org, "[discord]", "[policy]\n\"promotion.merit_rank_ceiling\" = 5\n\n[discord]");
    let s = Snapshot::parse(&files).unwrap();
    assert!(s.validate(today()).is_err());
    let cal = standing(&s, "cal", 75, today()).unwrap();
    assert_eq!(cal.rank, 5);
    assert_eq!(cal.tier.unwrap().rank, 5);
}

#[test]
fn tier_authority_is_filtered_at_read_time_even_from_a_bad_snapshot() {
    let files = edit(base(), F::Roles, "\"community.post\"]", "\"community.post\", \"payouts.approve\"]");
    let s = Snapshot::parse(&files).unwrap();
    assert!(s.validate(today()).is_err(), "validation refuses it");
    let cal = standing(&s, "cal", 75, today()).unwrap();
    assert!(!cal.permissions.contains("payouts.approve"));
    assert!(cal.permissions.contains("community.post"));
}

#[test]
fn suspension_keeps_only_profile_self_and_removal_keeps_nothing() {
    let files = edit(base(), F::Members, "id = \"lena\"\nname = \"Lena Lead\"\nstatus = \"active\"", "id = \"lena\"\nname = \"Lena Lead\"\nstatus = \"suspended\"");
    let s = load_ok(&files);
    let lena = standing(&s, "lena", 100, today()).unwrap();
    assert_eq!(lena.rank, 0);
    assert_eq!(lena.permissions.iter().cloned().collect::<Vec<_>>(), vec!["profile.self".to_string()]);
    assert!(lena.discord_roles.is_empty());
    let files = edit(base(), F::Members, "id = \"lena\"\nname = \"Lena Lead\"\nstatus = \"active\"", "id = \"lena\"\nname = \"Lena Lead\"\nstatus = \"removed\"");
    let lena = standing(&load_ok(&files), "lena", 100, today()).unwrap();
    assert!(lena.permissions.is_empty() && lena.discord_roles.is_empty() && lena.rank == 0);
}

#[test]
fn projection_is_the_full_set_of_pairs_and_the_rank_order() {
    let s = load_ok(&base());
    let merits = BTreeMap::from([("cal".to_string(), 60u64)]);
    let p = discord_projection(&s, &merits, today());
    let expected: std::collections::BTreeSet<(String, String)> = [
        ("900000000000000101", "900000000000000080"), // lena, Lead
        ("900000000000000103", "900000000000000060"), // mia, Maintainer
        ("900000000000000201", "900000000000000010"), // cal, Contributor tier
    ]
    .iter()
    .map(|(a, b)| (a.to_string(), b.to_string()))
    .collect();
    assert_eq!(p.assignments, expected);
    let order: Vec<u32> = p.role_order.iter().map(|r| r.rank).collect();
    assert_eq!(order, vec![80, 60, 40, 10]);
    // After mia's grant expires, her pair disappears.
    let later = discord_projection(&s, &merits, NaiveDate::from_ymd_opt(2027, 6, 2).unwrap());
    assert!(!later.assignments.iter().any(|(u, _)| u == "900000000000000103"));
}
