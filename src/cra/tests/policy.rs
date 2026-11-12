use std::collections::BTreeMap;

use super::*;
use crate::cra::policy::{ExternalEarn, PayoutEligible, Visibility, POLICY_KEYS};

/// Hierarchy doc 3.1: the mode is read in exactly one function. This scans
/// every `.rs` file under `src/cra/` for a read of a field named `mode` and
/// fails if one appears outside `policy::resolve_org_policy`.
#[test]
fn mode_is_read_in_exactly_one_function() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join("cra");
    let needle: String = [".", "mode"].concat();
    let mut files = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().map_or(false, |x| x == "rs") {
                files.push(p);
            }
        }
    }
    assert!(files.len() >= 8, "scan found too few files: {files:?}");

    let mut inside = 0;
    let mut outside = Vec::new();
    for path in &files {
        let text = std::fs::read_to_string(path).unwrap();
        // The byte range of `fn resolve_org_policy` in policy.rs, by brace matching.
        let allowed = if path.file_name().unwrap() == "policy.rs" {
            let start = text.find("pub fn resolve_org_policy(").expect("resolve_org_policy must exist");
            let open = start + text[start..].find('{').unwrap();
            let mut depth = 0;
            let mut end = open;
            for (i, c) in text[open..].char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = open + i;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            Some(start..end)
        } else {
            None
        };
        let mut from = 0;
        while let Some(pos) = text[from..].find(&needle) {
            let at = from + pos;
            from = at + needle.len();
            let next = text[from..].chars().next();
            if next.map_or(false, |c| c.is_alphanumeric() || c == '_') {
                continue; // `.mode_label` and similar are other fields
            }
            if allowed.as_ref().map_or(false, |r| r.contains(&at)) {
                inside += 1;
            } else {
                let line = text[..at].lines().count();
                outside.push(format!("{}:{line}", path.display()));
            }
        }
    }
    assert!(inside >= 1, "resolve_org_policy must read the mode");
    assert!(outside.is_empty(), "the mode is read outside resolve_org_policy at: {outside:?}");
}

#[test]
fn mode_defaults_match_hierarchy_doc_table() {
    let open = mode_defaults(Mode::Open);
    let hybrid = mode_defaults(Mode::Hybrid);
    let closed = mode_defaults(Mode::Closed);
    assert_eq!(
        (open.grants_two_person_min_rank, hybrid.grants_two_person_min_rank, closed.grants_two_person_min_rank),
        (60, 50, 30)
    );
    assert_eq!(
        (open.promotion_merit_rank_ceiling, hybrid.promotion_merit_rank_ceiling, closed.promotion_merit_rank_ceiling),
        (30, 30, 0)
    );
    assert_eq!((open.grants_max_term_days, hybrid.grants_max_term_days, closed.grants_max_term_days), (365, 180, 90));
    assert_eq!((open.payouts_hold_days, hybrid.payouts_hold_days, closed.payouts_hold_days), (7, 7, 14));
    assert_eq!(
        (open.payouts_two_person_min_cents, hybrid.payouts_two_person_min_cents, closed.payouts_two_person_min_cents),
        (50_000, 25_000, 0)
    );
    assert_eq!((open.probation_days, hybrid.probation_days, closed.probation_days), (14, 30, 90));
    assert_eq!(open.visibility_roster, Visibility::Public);
    assert_eq!(hybrid.visibility_roster, Visibility::Members);
    assert_eq!(closed.visibility_roster, Visibility::NeedToKnow);
    assert_eq!(closed.visibility_leaderboard, Visibility::Off);
    assert_eq!(closed.visibility_profile, Visibility::SelfAndChain);
    assert_eq!(hybrid.merit_external_contributions_earn, ExternalEarn::PublicReposOnly);
    assert_eq!(closed.merit_external_contributions_earn, ExternalEarn::No);
    assert_eq!(closed.payouts_eligible, PayoutEligible::PositionsOnly);
    assert!(!open.compartments_enabled && hybrid.compartments_enabled && closed.compartments_enabled);
    for p in [&open, &hybrid, &closed] {
        assert!(!p.promotion_merit_may_confer_authority);
        assert_eq!(p.consumers_subject_ids, "pairwise");
        assert_eq!(p.entries().len(), POLICY_KEYS.len());
    }
}

#[test]
fn overrides_win_field_by_field_in_both_toml_forms() {
    let mut o = BTreeMap::new();
    o.insert("payouts.hold_days".to_string(), toml::Value::Integer(10));
    let p = effective_policy(Mode::Open, &o).unwrap();
    assert_eq!(p.payouts_hold_days, 10);
    assert_eq!(p.grants_two_person_min_rank, 60, "other fields keep the mode default");

    // The unquoted dotted form `grants.two_person_min_rank = 70` is a nested table.
    let files = edit(base(), F::Org, "[discord]", "[policy]\ngrants.two_person_min_rank = 70\n\"payouts.hold_days\" = 9\n\n[discord]");
    let files = edit(files, F::Payouts, "hold_days = 7", "hold_days = 9");
    let s = load_ok(&files);
    assert_eq!(s.policy.grants_two_person_min_rank, 70);
    assert_eq!(s.policy.payouts_hold_days, 9);
}

#[test]
fn bad_policy_overrides_are_refused() {
    for (line, what) in [
        ("\"no.such_key\" = 1", "unknown key"),
        ("\"payouts.hold_days\" = \"ten\"", "wrong type"),
        ("\"visibility.leaderboard\" = \"need_to_know\"", "value outside the field's set"),
        ("\"consumers.subject_ids\" = \"raw\"", "only pairwise"),
        ("\"promotion.merit_rank_ceiling\" = 1000", "ceiling 1000"),
    ] {
        let files = edit(base(), F::Org, "[discord]", &format!("[policy]\n{line}\n\n[discord]"));
        let codes = error_codes(&files);
        assert!(codes.contains(&ErrorCode::PolicyOverride), "{what}: expected PolicyOverride, got {codes:?}");
    }
}

#[test]
fn content_hash_is_stable_and_covers_every_file_in_order() {
    let a = base();
    assert_eq!(a.content_hash(), base().content_hash());
    assert_eq!(a.content_hash().len(), 64);
    let s = load_ok(&a);
    assert_eq!(s.content_hash, a.content_hash());
    for f in [F::Org, F::Roles, F::Members, F::Merit, F::Payouts] {
        let b = append(base(), f, "\n");
        assert_ne!(a.content_hash(), b.content_hash());
    }
    // Moving bytes across a file boundary changes the hash (length framing).
    let x = CraFiles { org: "ab".into(), roles: "c".into(), ..Default::default() };
    let y = CraFiles { org: "a".into(), roles: "bc".into(), ..Default::default() };
    assert_ne!(x.content_hash(), y.content_hash());
}
