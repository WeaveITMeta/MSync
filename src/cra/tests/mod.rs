//! Tests for the CRA core. Fixtures are TOML text, edited per test, so every
//! test goes through the same parser a real repository does.

mod attacks;
mod authz;
mod examples;
mod plan;
mod policy;
mod standing;
mod validation;

use chrono::NaiveDate;

use super::*;

pub(super) fn today() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 11, 12).unwrap()
}

pub(super) const ORG: &str = r#"schema = 1

[org]
slug = "test-org"
name = "Test Org"
mode = "open"

[discord]
guild_id = "900000000000000000"

[[forge]]
provider = "github"
repo = "example/test"
visibility = "public"
canonical = true
payout_branches = ["main"]
"#;

pub(super) const ROLES: &str = r#"schema = 1

[[role]]
key = "newcomer"
title = "Newcomer"
kind = "tier"
rank = 0
min_merit = 0
permissions = ["profile.self", "community.read"]

[[role]]
key = "contributor"
title = "Contributor"
kind = "tier"
rank = 10
min_merit = 50
permissions = ["profile.self", "community.read", "community.post"]
discord_role = "900000000000000010"

[[role]]
key = "reviewer"
title = "Reviewer"
kind = "position"
rank = 40
permissions = ["members.grant", "members.approve_grant", "community.moderate"]
discord_role = "900000000000000040"

[[role]]
key = "maintainer"
title = "Maintainer"
kind = "position"
rank = 60
permissions = ["members.invite", "members.grant", "members.approve_grant", "members.revoke", "members.suspend"]
discord_role = "900000000000000060"

[[role]]
key = "lead"
title = "Lead Maintainer"
kind = "position"
rank = 80
permissions = ["members.invite", "members.grant", "members.approve_grant", "members.revoke", "members.suspend", "policy.propose_change", "policy.approve_change"]
discord_role = "900000000000000080"
"#;

pub(super) const MEMBERS: &str = r#"schema = 1

[[member]]
id = "root"
name = "Test Owner"
status = "active"
[member.identities]
github = "test-owner"
# end:root

[[member]]
id = "lena"
name = "Lena Lead"
status = "active"
[member.identities]
github = "lena-gh"
discord = "900000000000000101"
[[member.grant]]
role = "lead"
granted_by = "root"
expires = "2027-06-01"
# end:lena

[[member]]
id = "lars"
name = "Lars Lead"
status = "active"
[member.identities]
github = "lars-gh"
[[member.grant]]
role = "lead"
granted_by = "root"
expires = "2027-06-01"
# end:lars

[[member]]
id = "mia"
name = "Mia Maintainer"
status = "active"
[member.identities]
github = "mia-gh"
discord = "900000000000000103"
[[member.grant]]
role = "maintainer"
granted_by = "lena"
expires = "2027-06-01"
# end:mia

[[member]]
id = "max"
name = "Max Maintainer"
status = "active"
[member.identities]
github = "max-gh"
[[member.grant]]
role = "maintainer"
granted_by = "lena"
expires = "2027-06-01"
# end:max

[[member]]
id = "rex"
name = "Rex Reviewer"
status = "active"
[member.identities]
github = "rex-gh"
[[member.grant]]
role = "reviewer"
granted_by = "mia"
# end:rex

[[member]]
id = "ria"
name = "Ria Reviewer"
status = "active"
[member.identities]
github = "ria-gh"
[[member.grant]]
role = "reviewer"
granted_by = "mia"
# end:ria

[[member]]
id = "cal"
name = "Cal Contributor"
status = "active"
[member.identities]
github = "cal-gh"
discord = "900000000000000201"
# end:cal
"#;

pub(super) const MERIT: &str = r#"schema = 1
rubric = "dsync-rubric-v1"
scorers = ["agent"]
min_scorers = 1
score_window_days = 7

[[event]]
key = "pr_merged"
base_points = 10
max_points = 60
weight = 1.0
daily_cap = 150
vesting_days = 14
external_counts = true
"#;

pub(super) const PAYOUTS: &str = r#"schema = 1
enabled = true
currency = "usd"

[schedule]
cadence = "weekly"
day = "friday"
hour_utc = 17
mode = "prepare"

[limits]
min_payout_cents = 500
max_per_payout_cents = 10000
max_per_member_per_period_cents = 50000
max_per_run_cents = 250000
hold_days = 7

[approval]
approvals_required = 2
min_approver_rank = 60

[eligibility]
min_rank = 0
roles = []
require_verified_identity = true

[[rate]]
branch = "main"
base_cents = 1000
merit_multiplier = 0.01
"#;

pub(super) fn base() -> CraFiles {
    CraFiles {
        org: ORG.into(),
        roles: ROLES.into(),
        members: MEMBERS.into(),
        merit: MERIT.into(),
        payouts: PAYOUTS.into(),
    }
}

#[derive(Clone, Copy)]
pub(super) enum F {
    Org,
    Roles,
    Members,
    Merit,
    Payouts,
}

fn slot(files: &mut CraFiles, f: F) -> &mut String {
    match f {
        F::Org => &mut files.org,
        F::Roles => &mut files.roles,
        F::Members => &mut files.members,
        F::Merit => &mut files.merit,
        F::Payouts => &mut files.payouts,
    }
}

/// Replaces the first `from` with `to` in one file. Panics if `from` is
/// absent, so a fixture edit can never silently do nothing.
pub(super) fn edit(mut files: CraFiles, f: F, from: &str, to: &str) -> CraFiles {
    let s = slot(&mut files, f);
    assert!(s.contains(from), "fixture edit: `{from}` not found");
    *s = s.replacen(from, to, 1);
    files
}

/// Appends text to one file.
pub(super) fn append(mut files: CraFiles, f: F, text: &str) -> CraFiles {
    slot(&mut files, f).push_str(text);
    files
}

/// Adds a grant to an existing member of the base fixture.
pub(super) fn add_grant(files: CraFiles, member: &str, role: &str, by: &str, expires: Option<&str>) -> CraFiles {
    let exp = expires.map(|e| format!("expires = \"{e}\"\n")).unwrap_or_default();
    let marker = format!("# end:{member}");
    edit(
        files,
        F::Members,
        &marker,
        &format!("[[member.grant]]\nrole = \"{role}\"\ngranted_by = \"{by}\"\n{exp}{marker}"),
    )
}

pub(super) fn load_ok(files: &CraFiles) -> Snapshot {
    match load(files, today()) {
        Ok(s) => s,
        Err(e) => panic!("fixture should load, got errors:\n{}", e.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("\n")),
    }
}

/// Every error code from parse and validate.
pub(super) fn error_codes(files: &CraFiles) -> Vec<ErrorCode> {
    match load(files, today()) {
        Ok(_) => Vec::new(),
        Err(e) => e.into_iter().map(|e| e.code).collect(),
    }
}

pub(super) fn assert_refused(files: &CraFiles, code: ErrorCode) {
    let codes = error_codes(files);
    assert!(codes.contains(&code), "expected {code:?}, got {codes:?}");
}

pub(super) fn gh(login: &str) -> ForgeIdentity {
    ForgeIdentity::github(login)
}

pub(super) fn owners() -> Vec<String> {
    vec!["github:test-owner".to_string()]
}

pub(super) fn push(actor: &str) -> ChangeContext {
    ChangeContext { actor: gh(actor), via_pull_request: false, author: None, approvers: vec![], today: today() }
}

pub(super) fn pr(actor: &str, author: &str, approvers: &[&str]) -> ChangeContext {
    ChangeContext {
        actor: gh(actor),
        via_pull_request: true,
        author: Some(gh(author)),
        approvers: approvers.iter().map(|a| gh(a)).collect(),
        today: today(),
    }
}

/// Loads `new` (it must validate), plans against `old`, authorizes.
pub(super) fn auth(old: &Snapshot, new: &CraFiles, ctx: &ChangeContext) -> Result<(), Vec<Violation>> {
    let new = load_ok(new);
    let plan = plan_change(Some(old), &new);
    assert!(!plan.is_empty(), "test change produced an empty plan");
    authorize(Some(old), &plan, ctx, &owners())
}

pub(super) fn vcodes(r: &Result<(), Vec<Violation>>) -> Vec<ViolationCode> {
    match r {
        Ok(()) => Vec::new(),
        Err(v) => v.iter().map(|v| v.code).collect(),
    }
}

pub(super) fn assert_denied(r: &Result<(), Vec<Violation>>, code: ViolationCode) {
    let codes = vcodes(r);
    assert!(codes.contains(&code), "expected {code:?}, got {r:?}");
}

pub(super) fn assert_allowed(r: &Result<(), Vec<Violation>>) {
    assert!(r.is_ok(), "expected the change to be authorized, got {r:?}");
}

pub(super) fn example(slug: &str) -> CraFiles {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/cra").join(slug).join(".dsync");
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap_or_else(|e| panic!("{slug}/{name}: {e}"));
    CraFiles {
        org: read("org.toml"),
        roles: read("roles.toml"),
        members: read("members.toml"),
        merit: read("merit.toml"),
        payouts: read("payouts.toml"),
    }
}

#[test]
fn base_fixture_loads() {
    let s = load_ok(&base());
    assert_eq!(s.org.org.slug, "test-org");
    assert_eq!(s.members.members.len(), 8);
}
