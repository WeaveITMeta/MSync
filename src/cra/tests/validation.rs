//! One test per validation rule. Each starts from the valid base fixture and
//! breaks exactly one thing.

use super::*;
use ErrorCode as E;

// ------------------------------------------------------------- parsing

#[test]
fn schema_is_required_and_must_be_1() {
    assert_refused(&edit(base(), F::Roles, "schema = 1", ""), E::SchemaVersion);
    assert_refused(&edit(base(), F::Merit, "schema = 1", "schema = 2"), E::SchemaVersion);
}

#[test]
fn unknown_keys_are_refused_in_every_file() {
    assert_refused(&edit(base(), F::Org, "name = \"Test Org\"", "name = \"Test Org\"\ncolour = \"red\""), E::UnknownKey);
    assert_refused(&edit(base(), F::Roles, "title = \"Reviewer\"", "title = \"Reviewer\"\nsuperuser = true"), E::UnknownKey);
    assert_refused(&edit(base(), F::Members, "github = \"cal-gh\"", "github = \"cal-gh\"\ntwitter = \"cal\""), E::UnknownKey);
    assert_refused(&edit(base(), F::Merit, "daily_cap = 150", "daily_cap = 150\nbonus = 5"), E::UnknownKey);
    assert_refused(&append(base(), F::Payouts, "\n[extra]\nx = 1\n"), E::UnknownKey);
}

#[test]
fn syntax_errors_and_wrong_types_are_refused() {
    assert_refused(&append(base(), F::Org, "\nthis is not toml\n"), E::Parse);
    assert_refused(&edit(base(), F::Roles, "rank = 40", "rank = \"forty\""), E::Parse);
    assert_refused(&edit(base(), F::Org, "mode = \"open\"", "mode = \"secret\""), E::Parse);
    // A negative rank cannot even be represented.
    assert_refused(&edit(base(), F::Roles, "rank = 40", "rank = -1"), E::Parse);
}

#[test]
fn every_error_is_returned_not_just_the_first() {
    let files = edit(base(), F::Roles, "rank = 40", "rank = 1000");
    let files = edit(files, F::Payouts, "approvals_required = 2", "approvals_required = 0");
    let files = add_grant(files, "cal", "contributor", "lena", None);
    let codes = error_codes(&files);
    for c in [E::RankOutOfRange, E::ApprovalsRequiredTooLow, E::GrantToTier] {
        assert!(codes.contains(&c), "missing {c:?} in {codes:?}");
    }
    // Parse errors in several files are all reported too.
    let files = edit(base(), F::Roles, "schema = 1", "schema = 3");
    let files = append(files, F::Merit, "\nnope = = 1\n");
    let codes = error_codes(&files);
    assert!(codes.contains(&E::SchemaVersion) && codes.contains(&E::Parse), "{codes:?}");
}

// ------------------------------------------------------------- org.toml

#[test]
fn org_slug_must_be_a_key() {
    assert_refused(&edit(base(), F::Org, "slug = \"test-org\"", "slug = \"Test Org\""), E::InvalidKey);
}

#[test]
fn discord_ids_must_be_snowflakes() {
    assert_refused(&edit(base(), F::Org, "guild_id = \"900000000000000000\"", "guild_id = \"my-server\""), E::InvalidSnowflake);
    assert_refused(&edit(base(), F::Roles, "discord_role = \"900000000000000040\"", "discord_role = \"Reviewers\""), E::InvalidSnowflake);
    assert_refused(&edit(base(), F::Members, "discord = \"900000000000000201\"", "discord = \"cal#1234\""), E::InvalidSnowflake);
}

#[test]
fn duplicate_foundry_and_bad_repo_are_refused() {
    let dup = "\n[[foundry]]\nprovider = \"github\"\nrepo = \"Example/Test\"\nvisibility = \"public\"\ncanonical = true\n";
    assert_refused(&append(base(), F::Org, dup), E::DuplicateFoundry);
    let bad = "\n[[foundry]]\nprovider = \"gitlab\"\nrepo = \"just-a-name\"\nvisibility = \"public\"\ncanonical = true\n";
    assert_refused(&append(base(), F::Org, bad), E::InvalidRepo);
}

#[test]
fn canonical_is_required_and_a_mirror_cannot_pay() {
    assert_refused(&edit(base(), F::Org, "canonical = true\n", ""), E::Parse);
    let mirror = "\n[[foundry]]\nprovider = \"gitlab\"\nrepo = \"example/test-mirror\"\nvisibility = \"public\"\ncanonical = false\npayout_branches = [\"main\"]\n";
    assert_refused(&append(base(), F::Org, mirror), E::MirrorPaysOut);
}

#[test]
fn consumer_scopes_fields_and_keys_are_checked() {
    let c = |scopes: &str, fields: &str| {
        format!("\n[[consumer]]\nkey = \"site\"\nlabel = \"Site\"\nscopes = {scopes}\nfields = {fields}\n")
    };
    assert!(error_codes(&append(base(), F::Org, &c("[\"community.*\", \"profile.self\"]", "[\"tier\"]"))).is_empty());
    assert_refused(&append(base(), F::Org, &c("[\"applicants.*\"]", "[\"tier\"]")), E::UnknownScope);
    assert_refused(&append(base(), F::Org, &c("[\"community.delete_all\"]", "[\"tier\"]")), E::UnknownScope);
    assert_refused(&append(base(), F::Org, &c("[\"community.*\"]", "[\"email\"]")), E::UnknownConsumerField);
    let two = format!("{}{}", c("[\"community.*\"]", "[\"tier\"]"), c("[\"community.*\"]", "[\"tier\"]"));
    assert_refused(&append(base(), F::Org, &two), E::DuplicateConsumer);
}

#[test]
fn permissions_extra_extend_the_catalog_but_cannot_shadow_it() {
    let extra = "\n[[permissions_extra]]\nkey = \"community.mentor\"\nclass = \"basic\"\n";
    let files = edit(append(base(), F::Org, extra), F::Roles, "\"community.moderate\"]", "\"community.moderate\", \"community.mentor\"]");
    assert!(error_codes(&files).is_empty(), "{:?}", error_codes(&files));
    let shadow = "\n[[permissions_extra]]\nkey = \"members.grant\"\nclass = \"basic\"\n";
    assert_refused(&append(base(), F::Org, shadow), E::DuplicatePermission);
    let bad = "\n[[permissions_extra]]\nkey = \"Mentor\"\nclass = \"basic\"\n";
    assert_refused(&append(base(), F::Org, bad), E::InvalidPermission);
}

#[test]
fn extra_authority_permission_cannot_be_smuggled_onto_a_tier() {
    let extra = "\n[[permissions_extra]]\nkey = \"vault.open\"\nclass = \"authority\"\n";
    let files = edit(append(base(), F::Org, extra), F::Roles, "\"community.post\"]", "\"community.post\", \"vault.open\"]");
    assert_refused(&files, E::TierAuthorityPermission);
}

// ------------------------------------------------------------- roles.toml

#[test]
fn rank_1000_is_refused_in_the_repository() {
    assert_refused(&edit(base(), F::Roles, "rank = 80", "rank = 1000"), E::RankOutOfRange);
    assert_refused(&edit(base(), F::Roles, "rank = 80", "rank = 5000"), E::RankOutOfRange);
    assert!(error_codes(&edit(base(), F::Roles, "rank = 80", "rank = 999")).is_empty());
}

#[test]
fn a_tier_may_not_hold_an_authority_permission() {
    let files = edit(base(), F::Roles, "\"community.post\"]", "\"community.post\", \"members.grant\"]");
    assert_refused(&files, E::TierAuthorityPermission);
    // Sensitive is not authority: allowed on a tier.
    let files = edit(base(), F::Roles, "\"community.post\"]", "\"community.post\", \"merit.read_all\"]");
    assert!(error_codes(&files).is_empty());
}

#[test]
fn a_tier_may_hold_authority_only_when_policy_allows_it() {
    let files = edit(base(), F::Roles, "\"community.post\"]", "\"community.post\", \"members.grant\"]");
    let files = edit(files, F::Org, "[discord]", "[policy]\n\"promotion.merit_may_confer_authority\" = true\n\n[discord]");
    assert!(error_codes(&files).is_empty(), "{:?}", error_codes(&files));
}

#[test]
fn a_tier_above_the_merit_rank_ceiling_is_refused() {
    assert_refused(&edit(base(), F::Roles, "rank = 10", "rank = 31"), E::TierAboveCeiling);
    assert!(error_codes(&edit(base(), F::Roles, "rank = 10", "rank = 30")).is_empty());
    // Closed mode has ceiling 0, so the base tier at rank 10 is refused.
    let closed = edit(base(), F::Org, "mode = \"open\"", "mode = \"closed\"");
    assert_refused(&closed, E::TierAboveCeiling);
}

#[test]
fn a_tier_needs_min_merit() {
    assert_refused(&edit(base(), F::Roles, "min_merit = 50\n", ""), E::TierMissingMinMerit);
}

#[test]
fn position_only_and_tier_only_fields_stay_on_their_kind() {
    for field in ["max_holders = 3", "term_days = 30", "merit_eligibility = 100"] {
        let files = edit(base(), F::Roles, "min_merit = 50", &format!("min_merit = 50\n{field}"));
        assert_refused(&files, E::PositionOnlyField);
    }
    assert_refused(&edit(base(), F::Roles, "rank = 40", "rank = 40\nmin_merit = 5"), E::TierOnlyField);
}

#[test]
fn duplicate_role_keys_are_refused() {
    let dup = "\n[[role]]\nkey = \"reviewer\"\ntitle = \"Other\"\nkind = \"position\"\nrank = 20\n";
    assert_refused(&append(base(), F::Roles, dup), E::DuplicateRoleKey);
}

#[test]
fn duplicate_discord_role_ids_are_refused() {
    let files = edit(base(), F::Roles, "discord_role = \"900000000000000060\"", "discord_role = \"900000000000000040\"");
    assert_refused(&files, E::DuplicateDiscordRole);
}

#[test]
fn the_everyone_role_cannot_be_managed() {
    let files = edit(base(), F::Roles, "discord_role = \"900000000000000060\"", "discord_role = \"900000000000000000\"");
    assert_refused(&files, E::InvalidValue);
}

#[test]
fn unknown_permissions_are_refused() {
    assert_refused(&edit(base(), F::Roles, "\"community.moderate\"]", "\"community.moderate\", \"god.power\"]"), E::UnknownPermission);
}

#[test]
fn role_compartments_must_be_declared_and_enabled() {
    let files = edit(base(), F::Roles, "rank = 40", "rank = 40\ncompartments = [\"cell-eng\"]");
    assert_refused(&files, E::UnknownCompartment);
    // Declaring a compartment while the policy has compartments disabled (open default).
    let decl = "\n[[compartment]]\nkey = \"cell-eng\"\nlabel = \"Cell\"\nclassification = \"confidential\"\n";
    assert_refused(&append(files.clone(), F::Org, decl), E::CompartmentsDisabled);
    let on = edit(append(files, F::Org, decl), F::Org, "[discord]", "[policy]\n\"compartments.enabled\" = true\n\n[discord]");
    assert!(error_codes(&on).is_empty(), "{:?}", error_codes(&on));
}

#[test]
fn a_two_person_position_term_may_not_exceed_policy() {
    let files = edit(base(), F::Roles, "rank = 80", "rank = 80\nterm_days = 400");
    assert_refused(&files, E::TermExceedsPolicy);
}

// ------------------------------------------------------------ members.toml

#[test]
fn duplicate_member_ids_are_refused() {
    let dup = "\n[[member]]\nid = \"cal\"\nname = \"Other Cal\"\nstatus = \"active\"\n";
    assert_refused(&append(base(), F::Members, dup), E::DuplicateMemberId);
}

#[test]
fn an_identity_belongs_to_at_most_one_member() {
    // GitHub logins are case-insensitive.
    let dup = "\n[[member]]\nid = \"eve\"\nname = \"Eve\"\nstatus = \"active\"\n[member.identities]\ngithub = \"LENA-GH\"\n";
    assert_refused(&append(base(), F::Members, dup), E::DuplicateIdentity);
    let dup = "\n[[member]]\nid = \"eve\"\nname = \"Eve\"\nstatus = \"active\"\n[member.identities]\ndiscord = \"900000000000000201\"\n";
    assert_refused(&append(base(), F::Members, dup), E::DuplicateIdentity);
}

#[test]
fn member_ids_must_be_keys() {
    assert_refused(&edit(base(), F::Members, "id = \"cal\"", "id = \"Cal Smith\""), E::InvalidKey);
}

#[test]
fn a_grant_to_a_tier_is_refused() {
    assert_refused(&add_grant(base(), "cal", "contributor", "lena", None), E::GrantToTier);
}

#[test]
fn a_grant_to_an_unknown_role_is_refused() {
    assert_refused(&add_grant(base(), "cal", "emperor", "lena", None), E::UnknownRole);
}

#[test]
fn granted_by_must_be_a_member_and_not_the_grantee() {
    assert_refused(&add_grant(base(), "cal", "reviewer", "nobody", None), E::UnknownGranter);
    assert_refused(&add_grant(base(), "cal", "reviewer", "cal", None), E::SelfGrantedBy);
}

#[test]
fn the_same_role_twice_on_one_member_is_refused() {
    assert_refused(&add_grant(base(), "rex", "reviewer", "lena", None), E::DuplicateGrant);
}

#[test]
fn a_two_person_grant_needs_an_expiry_within_max_term() {
    assert_refused(&add_grant(base(), "cal", "maintainer", "lena", None), E::GrantMissingExpiry);
    // Open mode: max_term_days 365 from 2026-11-12 is 2027-11-12.
    assert!(error_codes(&add_grant(base(), "cal", "maintainer", "lena", Some("2027-11-12"))).is_empty());
    assert_refused(&add_grant(base(), "cal", "maintainer", "lena", Some("2027-11-13")), E::GrantTermTooLong);
    // Below the threshold an expiry is optional.
    assert!(error_codes(&add_grant(base(), "cal", "reviewer", "lena", None)).is_empty());
}

#[test]
fn a_role_term_days_bounds_its_grants() {
    let files = edit(base(), F::Roles, "rank = 40", "rank = 40\nterm_days = 30");
    // rex and ria hold reviewer with no expiry.
    assert_refused(&files, E::GrantMissingExpiry);
}

#[test]
fn a_toml_date_literal_is_accepted_for_expires() {
    let files = add_grant(base(), "cal", "maintainer", "lena", None);
    let files = edit(files, F::Members, "granted_by = \"lena\"\n# end:cal", "granted_by = \"lena\"\nexpires = 2027-01-01\n# end:cal");
    assert!(error_codes(&files).is_empty(), "{:?}", error_codes(&files));
}

#[test]
fn max_holders_is_enforced_on_live_grants() {
    let files = edit(base(), F::Roles, "rank = 60", "rank = 60\nmax_holders = 1");
    assert_refused(&files, E::MaxHoldersExceeded);
}

// ------------------------------------------------------------- merit.toml

#[test]
fn an_unknown_rubric_is_refused() {
    assert_refused(&edit(base(), F::Merit, "msync-rubric-v1", "my-rubric"), E::UnknownRubric);
}

#[test]
fn scorer_rules() {
    assert_refused(&edit(base(), F::Merit, "scorers = [\"agent\"]", "scorers = [\"agent\", \"agent\"]"), E::DuplicateScorer);
    assert_refused(&edit(base(), F::Merit, "min_scorers = 1", "min_scorers = 2"), E::MinScorers);
    assert_refused(&edit(base(), F::Merit, "scorers = [\"agent\"]", "scorers = [\"oracle\"]"), E::Parse);
    // Empty scorers means base points only; min_scorers is then moot.
    assert!(error_codes(&edit(base(), F::Merit, "scorers = [\"agent\"]", "scorers = []")).is_empty());
}

#[test]
fn event_rules() {
    let dup = "\n[[event]]\nkey = \"pr_merged\"\nbase_points = 1\nmax_points = 2\nweight = 1.0\ndaily_cap = 1\nvesting_days = 1\nexternal_counts = false\n";
    assert_refused(&append(base(), F::Merit, dup), E::DuplicateEvent);
    assert_refused(&edit(base(), F::Merit, "base_points = 10", "base_points = 70"), E::InvalidValue);
    assert_refused(&edit(base(), F::Merit, "weight = 1.0", "weight = -1.0"), E::InvalidValue);
    assert_refused(&edit(base(), F::Merit, "key = \"pr_merged\"", "key = \"pr_opened\""), E::Parse);
}

#[test]
fn external_merit_must_follow_policy() {
    let files = edit(base(), F::Org, "[discord]", "[policy]\n\"merit.external_contributions_earn\" = false\n\n[discord]");
    assert_refused(&files, E::ExternalMeritDisallowed);
}

// ------------------------------------------------------------ payouts.toml

#[test]
fn approvals_required_below_1_is_refused() {
    assert_refused(&edit(base(), F::Payouts, "approvals_required = 2", "approvals_required = 0"), E::ApprovalsRequiredTooLow);
    assert!(error_codes(&edit(base(), F::Payouts, "approvals_required = 2", "approvals_required = 1")).is_empty());
}

#[test]
fn schedule_rules() {
    assert_refused(&edit(base(), F::Payouts, "hour_utc = 17", "hour_utc = 24"), E::InvalidSchedule);
    assert_refused(&edit(base(), F::Payouts, "day = \"friday\"", "day = \"funday\""), E::InvalidSchedule);
    let monthly = edit(base(), F::Payouts, "cadence = \"weekly\"", "cadence = \"monthly\"");
    assert_refused(&monthly, E::InvalidSchedule);
    assert!(error_codes(&edit(monthly.clone(), F::Payouts, "day = \"friday\"", "day = 28")).is_empty());
    assert_refused(&edit(monthly, F::Payouts, "day = \"friday\"", "day = 29"), E::InvalidSchedule);
}

#[test]
fn limit_rules() {
    assert_refused(&edit(base(), F::Payouts, "min_payout_cents = 500", "min_payout_cents = 20000"), E::InvalidLimits);
    assert_refused(&edit(base(), F::Payouts, "max_per_run_cents = 250000", "max_per_run_cents = 100"), E::InvalidLimits);
    assert_refused(&edit(base(), F::Payouts, "hold_days = 7", "hold_days = 6"), E::HoldBelowPolicy);
    assert_refused(&edit(base(), F::Payouts, "currency = \"usd\"", "currency = \"US$\""), E::InvalidValue);
}

#[test]
fn eligibility_roles_must_exist_and_follow_policy() {
    assert_refused(&edit(base(), F::Payouts, "roles = []", "roles = [\"pope\"]"), E::UnknownRole);
    let files = edit(base(), F::Payouts, "roles = []", "roles = [\"contributor\"]");
    let files = edit(files, F::Org, "[discord]", "[policy]\n\"payouts.eligible\" = \"positions_only\"\n\n[discord]");
    assert_refused(&files, E::EligibilityConflictsPolicy);
}

#[test]
fn rate_rules() {
    assert_refused(&edit(base(), F::Payouts, "branch = \"main\"", "branch = \"dev\""), E::RateBranchNotPayable);
    let dup = "\n[[rate]]\nbranch = \"main\"\nbase_cents = 1\nmerit_multiplier = 0.0\n";
    assert_refused(&append(base(), F::Payouts, dup), E::DuplicateRate);
    assert_refused(&edit(base(), F::Payouts, "merit_multiplier = 0.01", "merit_multiplier = -0.5"), E::InvalidValue);
}
