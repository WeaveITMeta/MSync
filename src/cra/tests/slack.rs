//! Slack in the CRA (contract 12.2): `[slack]`, `slack_usergroup`, the `slack` identity,
//! their validation, their authorization (exactly like their Discord and GitHub twins), and
//! `slack_projection`.

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use ViolationCode as V;

const SLACK_ORG: &str = "[slack]\nteam_id = \"T9000000000\"\nalert_channel_id = \"C9000000009\"\n";

/// The base fixture with a Slack workspace beside Discord, a Slack user group on every role
/// that has a Discord role, and Slack ids for lena, mia, rex and cal.
fn with_slack() -> CraFiles {
    let mut f = append(base(), F::Org, &format!("\n{SLACK_ORG}"));
    for n in ["10", "40", "60", "80"] {
        let d = format!("discord_role = \"9000000000000000{n}\"");
        f = edit(
            f,
            F::Roles,
            &d,
            &format!("{d}\nslack_usergroup = \"S90000000{n}\""),
        );
    }
    for (anchor, id) in [
        ("discord = \"900000000000000101\"", "U9000000101"),
        ("discord = \"900000000000000103\"", "U9000000103"),
        ("github = \"rex-gh\"", "U9000000105"),
        ("discord = \"900000000000000201\"", "U9000000201"),
    ] {
        f = edit(
            f,
            F::Members,
            anchor,
            &format!("{anchor}\nslack = \"{id}\""),
        );
    }
    f
}

fn slack_only(files: CraFiles) -> CraFiles {
    edit(
        files,
        F::Org,
        "[discord]\nguild_id = \"900000000000000000\"\n",
        SLACK_ORG,
    )
}

// ------------------------------------------------------------- validation

#[test]
fn slack_only_discord_only_and_both_validate() {
    let discord_only = load_ok(&base());
    assert!(discord_only.org.discord.is_some() && discord_only.org.slack.is_none());
    let both = load_ok(&with_slack());
    assert!(both.org.discord.is_some());
    assert_eq!(
        both.org.slack.as_ref().map(|s| s.team_id.as_str()),
        Some("T9000000000")
    );
    let only = load_ok(&slack_only(base()));
    assert!(only.org.discord.is_none() && only.org.slack.is_some());
}

#[test]
fn an_org_with_neither_platform_is_refused() {
    let files = edit(
        base(),
        F::Org,
        "[discord]\nguild_id = \"900000000000000000\"\n",
        "",
    );
    assert_refused(&files, ErrorCode::NoChatPlatform);
    // Slack ids on members do not count as a platform.
    let files = edit(
        edit(
            with_slack(),
            F::Org,
            "[discord]\nguild_id = \"900000000000000000\"\n",
            "",
        ),
        F::Org,
        SLACK_ORG,
        "",
    );
    assert_refused(&files, ErrorCode::NoChatPlatform);
}

#[test]
fn malformed_slack_ids_are_refused() {
    let cases = [
        edit(
            with_slack(),
            F::Org,
            "team_id = \"T9000000000\"",
            "team_id = \"workspace\"",
        ),
        edit(
            with_slack(),
            F::Org,
            "alert_channel_id = \"C9000000009\"",
            "alert_channel_id = \"U9000000009\"",
        ),
        edit(
            with_slack(),
            F::Roles,
            "slack_usergroup = \"S9000000040\"",
            "slack_usergroup = \"C9000000040\"",
        ),
        edit(
            with_slack(),
            F::Members,
            "slack = \"U9000000103\"",
            "slack = \"u9000000103\"",
        ),
        edit(
            with_slack(),
            F::Members,
            "slack = \"U9000000103\"",
            "slack = \"U12\"",
        ),
        edit(
            with_slack(),
            F::Members,
            "slack = \"U9000000103\"",
            "slack = \"@mia\"",
        ),
    ];
    for files in &cases {
        assert_refused(files, ErrorCode::InvalidSlackId);
    }
    // A Slack id in the Discord field is still not a snowflake.
    assert_refused(
        &edit(
            with_slack(),
            F::Members,
            "discord = \"900000000000000103\"",
            "discord = \"U9000000103\"",
        ),
        ErrorCode::InvalidSnowflake,
    );
}

#[test]
fn duplicate_slack_ids_are_refused() {
    // Two members with one Slack id.
    let files = edit(
        with_slack(),
        F::Members,
        "slack = \"U9000000103\"",
        "slack = \"U9000000101\"",
    );
    assert_refused(&files, ErrorCode::DuplicateIdentity);
    // Two roles projecting onto one user group.
    let files = edit(
        with_slack(),
        F::Roles,
        "slack_usergroup = \"S9000000060\"",
        "slack_usergroup = \"S9000000040\"",
    );
    assert_refused(&files, ErrorCode::DuplicateSlackUsergroup);
    // An unknown key under [slack] fails closed like every other file.
    let files = edit(
        slack_only(base()),
        F::Org,
        "alert_channel_id = \"C9000000009\"\n",
        "alert_channel_id = \"C9000000009\"\nguild_id = \"x\"\n",
    );
    assert_refused(&files, ErrorCode::UnknownKey);
}

// ---------------------------------------------------------- authorization

/// Every context the identity rules distinguish.
fn contexts() -> Vec<(&'static str, ChangeContext)> {
    vec![
        ("push lena", push("lena-gh")),
        ("push mia", push("mia-gh")),
        ("push rex", push("rex-gh")),
        ("push cal", push("cal-gh")),
        (
            "pr lena approved by lars",
            pr("lena-gh", "lena-gh", &["lars-gh"]),
        ),
        (
            "pr lena approved by mia",
            pr("lena-gh", "lena-gh", &["mia-gh"]),
        ),
        (
            "pr mia approved by lena",
            pr("mia-gh", "mia-gh", &["lena-gh"]),
        ),
        (
            "pr rex approved by ria",
            pr("rex-gh", "rex-gh", &["ria-gh"]),
        ),
    ]
}

fn sorted(r: &Result<(), Vec<Violation>>) -> Vec<String> {
    let mut v: Vec<String> = vcodes(r).into_iter().map(|c| format!("{c:?}")).collect();
    v.sort();
    v
}

#[test]
fn a_slack_identity_swap_needs_exactly_the_authority_of_a_github_swap() {
    let old = load_ok(&with_slack());
    for (who, gh_from, gh_to, sl_from, sl_to) in [
        // mia: Maintainer, rank 60, two-person.
        (
            "mia",
            "github = \"mia-gh\"",
            "github = \"mia-new\"",
            "slack = \"U9000000103\"",
            "slack = \"U9000000999\"",
        ),
        // cal: no grant, protected up to the merit ceiling.
        (
            "cal",
            "github = \"cal-gh\"",
            "github = \"cal-new\"",
            "slack = \"U9000000201\"",
            "slack = \"U9000000999\"",
        ),
    ] {
        let gh_swap = edit(with_slack(), F::Members, gh_from, gh_to);
        let sl_swap = edit(with_slack(), F::Members, sl_from, sl_to);
        let plan = plan_change(Some(&old), &load_ok(&sl_swap));
        assert_eq!(plan.identity_changes.len(), 1, "{who}");
        assert_eq!(plan.identity_changes[0].field, "slack");
        for (label, ctx) in contexts() {
            let g = auth(&old, &gh_swap, &ctx);
            let s = auth(&old, &sl_swap, &ctx);
            assert_eq!(
                sorted(&g),
                sorted(&s),
                "{who}, {label}: GitHub {g:?} vs Slack {s:?}"
            );
        }
    }
    // The decisive cases, spelled out for mia's Slack id.
    let swap = edit(
        with_slack(),
        F::Members,
        "slack = \"U9000000103\"",
        "slack = \"U9000000999\"",
    );
    assert_denied(
        &auth(&old, &swap, &push("lena-gh")),
        V::DirectPushNeedsApproval,
    );
    assert_allowed(&auth(&old, &swap, &pr("lena-gh", "lena-gh", &["lars-gh"])));
    assert_denied(
        &auth(&old, &swap, &pr("lena-gh", "lena-gh", &["mia-gh"])),
        V::NoQualifiedApprover,
    );
    let own = auth(&old, &swap, &pr("mia-gh", "mia-gh", &["lena-gh"]));
    assert_denied(&own, V::SelfChange);
    assert_denied(&own, V::InsufficientRank);
}

#[test]
fn adding_a_slack_identity_is_an_identity_change_like_adding_discord() {
    let old = load_ok(&base());
    let add_slack = edit(
        base(),
        F::Members,
        "github = \"max-gh\"",
        "github = \"max-gh\"\nslack = \"U9000000104\"",
    );
    let add_discord = edit(
        base(),
        F::Members,
        "github = \"max-gh\"",
        "github = \"max-gh\"\ndiscord = \"900000000000000104\"",
    );
    for (label, ctx) in contexts() {
        assert_eq!(
            sorted(&auth(&old, &add_slack, &ctx)),
            sorted(&auth(&old, &add_discord, &ctx)),
            "{label}"
        );
    }
    assert_denied(
        &auth(&old, &add_slack, &push("lena-gh")),
        V::DirectPushNeedsApproval,
    );
}

#[test]
fn slack_role_and_org_changes_need_the_authority_of_their_discord_twins() {
    let old = load_ok(&with_slack());
    let role_sl = edit(
        with_slack(),
        F::Roles,
        "slack_usergroup = \"S9000000060\"",
        "slack_usergroup = \"S9000000061\"",
    );
    let role_dc = edit(
        with_slack(),
        F::Roles,
        "discord_role = \"900000000000000060\"",
        "discord_role = \"900000000000000061\"",
    );
    let org_sl = edit(
        with_slack(),
        F::Org,
        "alert_channel_id = \"C9000000009\"",
        "alert_channel_id = \"C9000000008\"",
    );
    let org_dc = edit(
        with_slack(),
        F::Org,
        "guild_id = \"900000000000000000\"",
        "guild_id = \"900000000000000000\"\nalert_channel_id = \"900000000000000009\"",
    );
    for (label, ctx) in contexts() {
        assert_eq!(
            sorted(&auth(&old, &role_sl, &ctx)),
            sorted(&auth(&old, &role_dc, &ctx)),
            "role, {label}"
        );
        assert_eq!(
            sorted(&auth(&old, &org_sl, &ctx)),
            sorted(&auth(&old, &org_dc, &ctx)),
            "org, {label}"
        );
    }
    let plan = plan_change(Some(&old), &load_ok(&org_sl));
    assert!(
        plan.org_changes
            .iter()
            .any(|c| c.path == "slack.alert_channel_id"),
        "{:?}",
        plan.org_changes
    );
    assert_denied(
        &auth(&old, &org_sl, &push("lena-gh")),
        V::DirectPushNeedsApproval,
    );
}

// ------------------------------------------------------------- projection

#[test]
fn slack_projection_matches_the_roles() {
    let s = load_ok(&with_slack());
    let merits = BTreeMap::from([("cal".to_string(), 75)]);
    let p = slack_projection(&s, &merits, today());
    let want: BTreeSet<(String, String)> = [
        ("U9000000101", "S9000000080"), // lena: Lead Maintainer
        ("U9000000103", "S9000000060"), // mia: Maintainer
        ("U9000000105", "S9000000040"), // rex: Reviewer
        ("U9000000201", "S9000000010"), // cal: Contributor tier at 75 merit
    ]
    .into_iter()
    .map(|(u, g)| (u.to_string(), g.to_string()))
    .collect();
    assert_eq!(p.assignments, want);
    let order: Vec<(&str, &str)> = p
        .usergroup_order
        .iter()
        .map(|g| (g.usergroup_id.as_str(), g.title.as_str()))
        .collect();
    assert_eq!(
        order,
        vec![
            ("S9000000080", "Lead Maintainer"),
            ("S9000000060", "Maintainer"),
            ("S9000000040", "Reviewer"),
            ("S9000000010", "Contributor")
        ]
    );
    // The same job titles, in the same order, as the Discord projection.
    let d = discord_projection(&s, &merits, today());
    let d_titles: Vec<&str> = d.role_order.iter().map(|r| r.title.as_str()).collect();
    let s_titles: Vec<&str> = p.usergroup_order.iter().map(|g| g.title.as_str()).collect();
    assert_eq!(d_titles, s_titles);
    // Every member with both ids holds the twin of each Discord role on Slack.
    let twin = |dr: &str| {
        s.roles
            .roles
            .iter()
            .find(|r| r.discord_role.as_deref() == Some(dr))
            .and_then(|r| r.slack_usergroup.clone())
    };
    for m in &s.members.members {
        if let (Some(du), Some(su)) = (&m.identities.discord, &m.identities.slack) {
            let from_discord: BTreeSet<String> = d
                .assignments
                .iter()
                .filter(|(u, _)| u == du)
                .filter_map(|(_, r)| twin(r))
                .collect();
            let on_slack: BTreeSet<String> = p
                .assignments
                .iter()
                .filter(|(u, _)| u == su)
                .map(|(_, g)| g.clone())
                .collect();
            assert_eq!(from_discord, on_slack, "{}", m.id);
        }
    }
    // Full membership per group, and an empty managed group is listed as empty.
    let by_group = p.users_by_usergroup();
    assert_eq!(
        by_group.get("S9000000060"),
        Some(&vec!["U9000000103".to_string()])
    );
    let p0 = slack_projection(&s, &BTreeMap::new(), today());
    assert_eq!(
        p0.users_by_usergroup().get("S9000000010"),
        Some(&Vec::new())
    );
}

#[test]
fn suspended_members_and_expired_grants_leave_their_user_groups() {
    let files = edit(
        with_slack(),
        F::Members,
        "id = \"mia\"\nname = \"Mia Maintainer\"\nstatus = \"active\"",
        "id = \"mia\"\nname = \"Mia Maintainer\"\nstatus = \"suspended\"",
    );
    let s = load_ok(&files);
    let p = slack_projection(&s, &BTreeMap::new(), today());
    assert!(!p.assignments.iter().any(|(u, _)| u == "U9000000103"));
    let later = slack_projection(
        &load_ok(&with_slack()),
        &BTreeMap::new(),
        chrono::NaiveDate::from_ymd_opt(2027, 6, 2).unwrap(),
    );
    assert!(
        !later
            .assignments
            .contains(&("U9000000101".to_string(), "S9000000080".to_string())),
        "lena's grant expired"
    );
}

#[test]
fn the_plan_reports_slack_user_group_impact() {
    let old = load_ok(&with_slack());
    let new = load_ok(&add_grant(with_slack(), "cal", "reviewer", "mia", None));
    let plan = plan_change(Some(&old), &new);
    assert_eq!(
        plan.impact.slack_usergroup_gains.get("S9000000040"),
        Some(&vec!["cal".to_string()])
    );
    assert!(plan
        .render_markdown()
        .contains("Slack user group `S9000000040`: 1 member(s) join it (cal)"));
}

// --------------------------------------------------------------- examples

#[test]
fn the_examples_cover_both_platforms_slack_only_and_discord_only() {
    let e = load_ok(&example("eustress-engine"));
    assert!(
        e.org.discord.is_some() && e.org.slack.is_some(),
        "eustress-engine uses both"
    );
    let w = load_ok(&example("weave-solutions"));
    assert!(
        w.org.discord.is_none() && w.org.slack.is_some(),
        "weave-solutions uses Slack only"
    );
    assert!(w.roles.roles.iter().all(|r| r.discord_role.is_none()));
    assert!(w
        .members
        .members
        .iter()
        .all(|m| m.identities.discord.is_none() && m.identities.slack.is_some()));
    let v = load_ok(&example("voltec-industries"));
    assert!(
        v.org.discord.is_some() && v.org.slack.is_none(),
        "voltec-industries uses Discord only"
    );
    assert!(v.roles.roles.iter().all(|r| r.slack_usergroup.is_none()));
    assert!(v
        .members
        .members
        .iter()
        .all(|m| m.identities.slack.is_none()));
    // McKale holds the same job title on both of Eustress's platforms.
    let proj = slack_projection(&e, &BTreeMap::new(), today());
    assert!(proj
        .assignments
        .contains(&("U1000000102".to_string(), "S1000000080".to_string())));
    let weave = slack_projection(&w, &BTreeMap::new(), today());
    assert!(
        weave
            .assignments
            .contains(&("U2000000102".to_string(), "S2000000070".to_string())),
        "McKale is Director at Weave"
    );
}

#[test]
fn every_example_slack_id_is_an_obvious_placeholder() {
    // Prefix, the org digit, then zeros and a three-digit suffix: T1000000000, U1000000102.
    let fake = |id: &str| {
        id.len() == 11 && id[2..8] == *"000000" && id[1..].chars().all(|c| c.is_ascii_digit())
    };
    for slug in ["eustress-engine", "weave-solutions", "voltec-industries"] {
        let s = load_ok(&example(slug));
        if let Some(sl) = &s.org.slack {
            assert!(fake(&sl.team_id), "{slug}: {}", sl.team_id);
            assert!(sl.alert_channel_id.as_deref().map_or(true, fake), "{slug}");
        }
        for r in &s.roles.roles {
            assert!(
                r.slack_usergroup.as_deref().map_or(true, fake),
                "{slug}: {:?}",
                r.slack_usergroup
            );
        }
        for m in &s.members.members {
            assert!(
                m.identities.slack.as_deref().map_or(true, fake),
                "{slug}: {:?}",
                m.identities.slack
            );
        }
    }
}
