use std::collections::BTreeMap;

use super::*;
use crate::cra::policy::{ExternalEarn, PayoutEligible, Visibility, POLICY_KEYS};

/// Hierarchy doc 3.1: the mode is read in exactly one function. This scans
/// every `.rs` file under `src/cra/` for a read of a field named `mode` and
/// fails if one appears outside `policy::resolve_org_policy`.
#[test]
fn mode_is_read_in_exactly_one_function() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("cra");
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
            let start = text
                .find("pub fn resolve_org_policy(")
                .expect("resolve_org_policy must exist");
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
    assert!(
        outside.is_empty(),
        "the mode is read outside resolve_org_policy at: {outside:?}"
    );
}

#[test]
fn mode_defaults_match_hierarchy_doc_table() {
    let open = mode_defaults(Mode::Open);
    let hybrid = mode_defaults(Mode::Hybrid);
    let closed = mode_defaults(Mode::Closed);
    assert_eq!(
        (
            open.grants_two_person_min_rank,
            hybrid.grants_two_person_min_rank,
            closed.grants_two_person_min_rank
        ),
        (60, 50, 30)
    );
    assert_eq!(
        (
            open.promotion_merit_rank_ceiling,
            hybrid.promotion_merit_rank_ceiling,
            closed.promotion_merit_rank_ceiling
        ),
        (30, 30, 0)
    );
    assert_eq!(
        (
            open.grants_max_term_days,
            hybrid.grants_max_term_days,
            closed.grants_max_term_days
        ),
        (365, 180, 90)
    );
    assert_eq!(
        (
            open.payouts_hold_days,
            hybrid.payouts_hold_days,
            closed.payouts_hold_days
        ),
        (7, 7, 14)
    );
    assert_eq!(
        (
            open.payouts_two_person_min_cents,
            hybrid.payouts_two_person_min_cents,
            closed.payouts_two_person_min_cents
        ),
        (50_000, 25_000, 0)
    );
    assert_eq!(
        (
            open.probation_days,
            hybrid.probation_days,
            closed.probation_days
        ),
        (14, 30, 90)
    );
    assert_eq!(open.visibility_roster, Visibility::Public);
    assert_eq!(hybrid.visibility_roster, Visibility::Members);
    assert_eq!(closed.visibility_roster, Visibility::NeedToKnow);
    assert_eq!(closed.visibility_leaderboard, Visibility::Off);
    assert_eq!(closed.visibility_profile, Visibility::SelfAndChain);
    assert_eq!(
        hybrid.merit_external_contributions_earn,
        ExternalEarn::PublicReposOnly
    );
    assert_eq!(closed.merit_external_contributions_earn, ExternalEarn::No);
    assert_eq!(closed.payouts_eligible, PayoutEligible::PositionsOnly);
    assert!(
        !open.compartments_enabled && hybrid.compartments_enabled && closed.compartments_enabled
    );
    for p in [&open, &hybrid, &closed] {
        assert!(!p.promotion_merit_may_confer_authority);
        assert_eq!(p.consumers_subject_ids, "pairwise");
        assert_eq!(p.entries().len(), POLICY_KEYS.len());
    }
}

/// Contract 3.1.1, the `source_available` column, one row per policy field.
const SOURCE_AVAILABLE_TABLE: &[(&str, &str)] = &[
    ("visibility.roster", "members"),
    ("visibility.leaderboard", "members"),
    ("visibility.profile", "members"),
    ("visibility.positions", "members"),
    ("merit.external_contributions_earn", "true"),
    ("merit.sources_enabled", "[*]"),
    ("promotion.merit_rank_ceiling", "30"),
    ("promotion.merit_may_confer_authority", "false"),
    ("promotion.eligibility_nominations", "true"),
    ("compartments.enabled", "true"),
    ("probation.days", "30"),
    ("payouts.eligible", "verified_members"),
    ("payouts.hold_days", "7"),
    ("payouts.two_person_min_cents", "25000"),
    ("grants.two_person_min_rank", "50"),
    ("grants.max_term_days", "180"),
    ("audit.visibility", "officers"),
    ("anomaly.profile_reads_per_hour", "120"),
    ("anomaly.action_on_trip", "alert"),
    ("consumers.subject_ids", "pairwise"),
];

#[test]
fn source_available_defaults_match_the_contract_table_field_by_field() {
    let p = mode_defaults(Mode::SourceAvailable);
    let got = p.entries();
    assert_eq!(
        SOURCE_AVAILABLE_TABLE.len(),
        POLICY_KEYS.len(),
        "the table covers every policy field"
    );
    assert_eq!(got.len(), SOURCE_AVAILABLE_TABLE.len());
    for ((key, want), (got_key, got_val)) in SOURCE_AVAILABLE_TABLE.iter().zip(&got) {
        assert_eq!(key, got_key, "field order");
        assert_eq!(got_val, want, "source_available `{key}`");
    }
    // The one documented difference from hybrid.
    let hybrid = mode_defaults(Mode::Hybrid);
    assert_eq!(p.merit_external_contributions_earn, ExternalEarn::Yes);
    assert_eq!(
        hybrid.merit_external_contributions_earn,
        ExternalEarn::PublicReposOnly
    );
    let differs: Vec<&str> = p
        .entries()
        .into_iter()
        .zip(hybrid.entries())
        .filter(|(a, b)| a != b)
        .map(|(a, _)| a.0)
        .collect();
    assert_eq!(differs, vec!["merit.external_contributions_earn"]);
    assert_eq!(
        (
            Mode::SourceAvailable.as_str(),
            Mode::SourceAvailable.label()
        ),
        ("source_available", "Source available")
    );
}

/// Every mode's defaults against the 3.1.1 table as written in
/// `docs/CRA_PLATFORM.md`, so the code and the contract cannot drift apart.
#[test]
fn every_mode_default_matches_the_table_in_the_platform_contract() {
    let doc = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs")
            .join("CRA_PLATFORM.md"),
    )
    .unwrap();
    let start = doc.find("### 3.1.1 Modes").expect("contract section 3.1.1");
    let rows: Vec<Vec<String>> = doc[start..]
        .lines()
        .skip_while(|l| !l.starts_with('|'))
        .take_while(|l| l.starts_with('|'))
        .map(|l| {
            l.trim_matches('|')
                .split('|')
                .map(|c| c.trim().replace('`', ""))
                .collect()
        })
        .collect();
    let header = &rows[0];
    let modes: Vec<Mode> = Mode::ALL.to_vec();
    assert_eq!(
        header[1..].to_vec(),
        modes
            .iter()
            .map(|m| m.as_str().to_string())
            .collect::<Vec<_>>()
    );
    let body: Vec<&Vec<String>> = rows[2..].iter().collect();
    assert_eq!(
        body.len(),
        POLICY_KEYS.len(),
        "one table row per policy field"
    );
    for (col, mode) in modes.iter().enumerate() {
        let entries = mode_defaults(*mode).entries();
        for (row, (key, value)) in body.iter().zip(&entries) {
            assert_eq!(&row[0], key, "row order");
            let cell = &row[col + 1];
            let want = match cell.as_str() {
                "all" => "[*]".to_string(),
                c if c.contains(" plus ") => {
                    format!("[{}]", c.split(" plus ").collect::<Vec<_>>().join(", "))
                }
                c => c.to_string(),
            };
            assert_eq!(value, &want, "{} `{key}`", mode.as_str());
        }
    }
}

/// The base fixture switched to `source_available`, with the two-person
/// grants moved inside its 180-day term limit.
fn source_available(files: CraFiles) -> CraFiles {
    let files = edit(
        files,
        F::Org,
        "mode = \"open\"",
        "mode = \"source_available\"",
    );
    within_180_days(files)
}

fn within_180_days(mut files: CraFiles) -> CraFiles {
    files.members = files
        .members
        .replace("expires = \"2027-06-01\"", "expires = \"2027-05-01\"");
    files
}

#[test]
fn source_available_parses_from_org_toml() {
    let s = load_ok(&source_available(base()));
    assert_eq!(s.mode_label, Mode::SourceAvailable);
    assert_eq!(s.policy, mode_defaults(Mode::SourceAvailable));
    // Overrides apply on top of it like any other mode.
    let files = edit(
        source_available(base()),
        F::Org,
        "[discord]",
        "[policy]\n\"payouts.hold_days\" = 9\n\n[discord]",
    );
    let files = edit(files, F::Payouts, "hold_days = 7", "hold_days = 9");
    let s = load_ok(&files);
    assert_eq!(
        (
            s.policy.payouts_hold_days,
            s.policy.grants_two_person_min_rank
        ),
        (9, 50)
    );
    // Its 180-day term limit applies: the base fixture's 2027-06-01 grants are too long.
    let long = edit(
        base(),
        F::Org,
        "mode = \"open\"",
        "mode = \"source_available\"",
    );
    assert_refused(&long, ErrorCode::GrantTermTooLong);
}

#[test]
fn unknown_modes_are_still_refused() {
    for bad in [
        "secret",
        "sourceavailable",
        "source-available",
        "Source available",
        "SourceAvailable",
        "source_available ",
    ] {
        let files = edit(
            base(),
            F::Org,
            "mode = \"open\"",
            &format!("mode = \"{bad}\""),
        );
        let codes = error_codes(&files);
        assert!(
            codes.contains(&ErrorCode::Parse),
            "`{bad}`: expected Parse, got {codes:?}"
        );
    }
}

#[test]
fn a_mode_change_to_or_from_source_available_is_two_person() {
    let with_mode = |m: &str| {
        within_180_days(edit(
            base(),
            F::Org,
            "mode = \"open\"",
            &format!("mode = \"{m}\""),
        ))
    };
    for (from, to) in [
        (Mode::Open, Mode::SourceAvailable),
        (Mode::SourceAvailable, Mode::Open),
        (Mode::SourceAvailable, Mode::Hybrid),
        (Mode::Hybrid, Mode::SourceAvailable),
    ] {
        let old = load_ok(&with_mode(from.as_str()));
        let new = with_mode(to.as_str());
        let plan = plan_change(Some(&old), &load_ok(&new));
        let m = plan.mode_change.as_ref().expect("mode change detected");
        assert_eq!((m.old, m.new), (Some(from), to));
        let item = format!("mode {} to {}", from.as_str(), to.as_str());

        // A direct push by a Lead (80) is refused: no second person.
        let r = auth(&old, &new, &push("lena-gh"));
        assert_denied(&r, ViolationCode::DirectPushNeedsApproval);
        let items: Vec<String> = r
            .unwrap_err()
            .into_iter()
            .filter(|v| v.code == ViolationCode::DirectPushNeedsApproval)
            .map(|v| v.item)
            .collect();
        assert!(items.contains(&item), "{item} not in {items:?}");
        // A pull request with the author as the only approver is refused too.
        assert!(
            auth(&old, &new, &pr("lena-gh", "lena-gh", &["lena-gh"])).is_err(),
            "{item}"
        );
        // Approved by a second Lead, it is authorized.
        assert_allowed(&auth(&old, &new, &pr("lena-gh", "lena-gh", &["lars-gh"])));
    }
}

#[test]
fn plan_change_previews_a_switch_to_source_available() {
    let old = load_ok(&within_180_days(base()));
    let new = load_ok(&source_available(base()));
    let p = plan_change(Some(&old), &new);
    let m = p.mode_change.as_ref().unwrap();
    assert_eq!((m.old, m.new), (Some(Mode::Open), Mode::SourceAvailable));
    let fields: Vec<&str> = p.policy_changes.iter().map(|c| c.path.as_str()).collect();
    for f in [
        "visibility.roster",
        "visibility.leaderboard",
        "compartments.enabled",
        "grants.two_person_min_rank",
        "grants.max_term_days",
    ] {
        assert!(fields.contains(&f), "{f} missing from {fields:?}");
    }
    assert!(
        !fields.contains(&"merit.external_contributions_earn"),
        "open and source_available both earn: {fields:?}"
    );
    // Two-person at the lower threshold (50), org changes above the higher (60).
    assert_eq!((p.two_person_min_rank, p.org_change_rank), (50, 60));
    let md = p.render_markdown();
    assert!(md.contains("`open` to `source_available`"), "{md}");
    assert!(md.contains("grants.two_person_min_rank"), "{md}");
    assert!(!md.contains('\u{2014}'), "no em dashes in the preview");
}

#[test]
fn overrides_win_field_by_field_in_both_toml_forms() {
    let mut o = BTreeMap::new();
    o.insert("payouts.hold_days".to_string(), toml::Value::Integer(10));
    let p = effective_policy(Mode::Open, &o).unwrap();
    assert_eq!(p.payouts_hold_days, 10);
    assert_eq!(
        p.grants_two_person_min_rank, 60,
        "other fields keep the mode default"
    );

    // The unquoted dotted form `grants.two_person_min_rank = 70` is a nested table.
    let files = edit(
        base(),
        F::Org,
        "[discord]",
        "[policy]\ngrants.two_person_min_rank = 70\n\"payouts.hold_days\" = 9\n\n[discord]",
    );
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
        (
            "\"visibility.leaderboard\" = \"need_to_know\"",
            "value outside the field's set",
        ),
        ("\"consumers.subject_ids\" = \"raw\"", "only pairwise"),
        ("\"promotion.merit_rank_ceiling\" = 1000", "ceiling 1000"),
    ] {
        let files = edit(
            base(),
            F::Org,
            "[discord]",
            &format!("[policy]\n{line}\n\n[discord]"),
        );
        let codes = error_codes(&files);
        assert!(
            codes.contains(&ErrorCode::PolicyOverride),
            "{what}: expected PolicyOverride, got {codes:?}"
        );
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
    let x = CraFiles {
        org: "ab".into(),
        roles: "c".into(),
        ..Default::default()
    };
    let y = CraFiles {
        org: "a".into(),
        roles: "bc".into(),
        ..Default::default()
    };
    assert_ne!(x.content_hash(), y.content_hash());
}
