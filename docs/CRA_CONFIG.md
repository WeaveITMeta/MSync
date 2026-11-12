# CRA configuration reference

The Central Rank Authority (CRA) is a set of five TOML files in `.dsync/` on the default branch of an organization's repository. It defines roles, job titles, ranks, permissions, members, merit rules and payout policy. A change to the hierarchy is a commit, reviewed like code. DSync parses the files, validates them, shows the change plan, and decides whether the person who made the change was allowed to.

This document is the reference for the file formats, every validation rule, and the change-authority rules. The build contract is `docs/CRA_PLATFORM.md` (sections 3 and 4). The code is `src/cra/`. Three complete examples live in `examples/cra/`.

## Contents

1. Files and general rules
2. `org.toml`
3. `roles.toml`
4. `members.toml`
5. `merit.toml`
6. `payouts.toml`
7. Policy fields and mode defaults
8. Permission catalog
9. Standing and the Discord projection
10. The change plan
11. Who may change what
12. Walkthrough: Voltec Industries
13. Choices this implementation made

## 1. Files and general rules

| File | Holds |
|---|---|
| `org.toml` | Identity of the organization, mode, policy overrides, Discord guild, forge repositories, portals (consumers), compartments, extra permissions |
| `roles.toml` | Tiers (earned) and positions (appointed), with rank, title, permissions and Discord role |
| `members.toml` | Members, their status, their identities, and the positions granted to them |
| `merit.toml` | Rubric, scorers and merit events |
| `payouts.toml` | Kill switch, schedule, limits, approvals, eligibility and rates |

- Every file starts with `schema = 1`. A missing or different schema is refused.
- Unknown keys are refused in every table. A typo fails closed instead of being ignored.
- All errors are returned at once, across all five files.
- Keys and ids (`org.slug`, role keys, member ids, consumer and compartment keys) use `[a-z0-9-]`, are not empty, and do not start or end with `-`.
- Discord ids are snowflakes: 17 to 20 digits, written as strings.
- Dates are `YYYY-MM-DD`, either quoted (`"2027-11-01"`) or as a TOML date (`2027-11-01`).
- The snapshot's content hash is SHA-256 over the five files in the order org, roles, members, merit, payouts. Each file is framed as its file name, a newline, its byte length, a newline, its bytes and a newline, so bytes cannot move from one file to the next without changing the hash.

Owners (rank 1000) are engine configuration (`DSYNC_OWNER_MEMBER_IDS`), never CRA content. A compromised repository cannot mint an owner.

## 2. `org.toml`

```toml
schema = 1

[org]
slug = "voltec-industries"     # required, [a-z0-9-]
name = "Voltec Industries"     # required, not empty
mode = "hybrid"                # required: open | hybrid | closed
template = "hybrid"            # optional, informational

[policy]                       # optional overrides of the mode defaults (section 7)
"payouts.hold_days" = 10       # quoted dotted key, or the TOML dotted form payouts.hold_days = 10

[[permissions_extra]]          # optional, permissions beyond the catalog
key = "community.mentor"       # surface.action
class = "basic"                # basic | sensitive | authority

[discord]                      # optional
guild_id = "300000000000000000"
alert_channel_id = "300000000000000009"   # optional

[[forge]]                      # repositories whose events count
provider = "gitlab"            # github | gitlab
host = "gitlab.com"            # optional; github.com or gitlab.com by default
repo = "example-voltec/cell-firmware"     # owner/name
visibility = "private"         # public | private
canonical = true               # required. A mirror sets false and earns nothing.
payout_branches = ["main"]

[[compartment]]                # optional need-to-know compartments
key = "cell-eng"
label = "Cell Engineering"
classification = "confidential"   # internal | confidential | restricted

[[consumer]]                   # portals allowed to read standing
key = "voltec-portal"
label = "Voltec community portal"
scopes = ["community.*"]
fields = ["tier", "positions.title", "merit.vested", "permissions"]
```

Validation rules:

- `org.slug` is a valid key. `org.name` is not empty. `org.mode` is one of the three modes.
- Every `[policy]` key is a known policy field (section 7), with the right type and a value from that field's allowed set. `consumers.subject_ids` accepts only `pairwise`.
- `discord.guild_id` and `discord.alert_channel_id` are snowflakes.
- `permissions_extra`: each key has the shape `surface.action`, is not already in the catalog, and is declared once.
- `forge`: `repo` is `owner/name`; `host`, when set, is a bare host name; the same provider, host and repository is listed once (case-insensitive); `canonical` is required; a repository with `canonical = false` may not list `payout_branches`.
- `consumer`: key valid and unique; each scope is either `surface.*` for a known surface or an exact known permission; each field is one of `status`, `tier`, `positions.title`, `positions.rank`, `positions.expires_at`, `merit.vested`, `permissions`.
- `compartment`: key valid and unique. Declaring a compartment while the effective policy has `compartments.enabled = false` is refused.

## 3. `roles.toml`

```toml
schema = 1

[[role]]
key = "contributor"            # [a-z0-9-], unique
title = "Contributor"          # the job title shown in Discord and portals
kind = "tier"                  # tier (earned from merit) | position (appointed)
rank = 10                      # 0..=999. 1000 is owners only.
permissions = ["community.post", "community.vote"]
discord_role = "300000000000000010"   # optional snowflake
min_merit = 50                 # tier only, required

[[role]]
key = "head-of-cell-engineering"
title = "Head of Cell Engineering"
kind = "position"
rank = 60
permissions = ["members.grant", "members.approve_grant", "members.revoke"]
discord_role = "300000000000000060"
max_holders = 1                # position only, optional
term_days = 180                # position only, optional
merit_eligibility = 1000       # position only, optional: files a nomination
compartments = ["cell-eng"]    # optional, must be declared in org.toml
```

Validation rules:

- Role keys are valid and unique. Titles are not empty.
- `rank` is 0 to 999. Rank 1000 or above is refused: it is owners only and comes from engine configuration.
- `discord_role` is a snowflake, unique across roles, and not the guild id (the `@everyone` role).
- Every permission is in the catalog (section 8) or in `permissions_extra`, and is listed once per role.
- A tier needs `min_merit`. A tier may not set `max_holders`, `term_days` or `merit_eligibility`. A position may not set `min_merit`.
- A tier may not hold an authority-class permission unless the effective policy sets `promotion.merit_may_confer_authority = true`. This includes extra permissions declared with `class = "authority"`.
- A tier's rank may not exceed `promotion.merit_rank_ceiling`.
- Every compartment a role names is declared in `org.toml`.
- A position at or above `grants.two_person_min_rank` may not set `term_days` above `grants.max_term_days`.
- `max_holders` and `term_days`, when set, are at least 1.

## 4. `members.toml`

```toml
schema = 1

[[member]]
id = "mckale"                  # stable handle, [a-z0-9-], unique
name = "McKale Olson"
status = "active"              # active | suspended | removed

[member.identities]            # attested by the administration in the CRA
github = "Simbuilder"
gitlab = "simbuilder"
discord = "300000000000000104"

[[member.grant]]               # appointed positions only; tiers are computed
role = "engineer"
granted_by = "gus-example"     # member id
expires = "2027-04-30"         # see the expiry rules below
```

Validation rules:

- Member ids are valid and unique. Names are not empty.
- An identity value belongs to at most one member. GitHub and GitLab logins compare case-insensitively; Discord ids compare exactly. Discord identities are snowflakes.
- `grant.role` names an existing role, and that role is a position. A grant to a tier is refused: tiers are computed from merit, never granted.
- `granted_by` names an existing member, and is not the grantee.
- A member holds a given role at most once.
- Expiry: a grant of a role at or above `grants.two_person_min_rank` needs `expires`, at most `grants.max_term_days` after the validation date. A grant of a role that sets `term_days` needs `expires`, at most `term_days` after the validation date. When both apply, the shorter limit wins. Because time only moves forward, a grant that was valid when loaded stays valid on every later load.
- `max_holders`: live grants (member not removed, grant not expired on the validation date) may not exceed it.

An identity listed here is proven by the administration. A contributor who is not in `members.toml` earns merit under their forge login and links Discord by OAuth proof, never by typing a username.

## 5. `merit.toml`

```toml
schema = 1
rubric = "dsync-rubric-v1"     # the engine refuses an id it does not know
scorers = ["agent"]            # agent | coderabbit. Empty = base points only.
min_scorers = 1
score_window_days = 7

[[event]]
key = "pr_merged"              # pr_merged | review_approved | issue_closed | release_published | docs_merged | manual_award
base_points = 10
max_points = 60
weight = 1.0
daily_cap = 150
vesting_days = 14
external_counts = true         # do non-members earn it
```

Validation rules:

- `rubric` is known to the engine (`dsync-rubric-v1`).
- Each scorer is listed once. When scorers are listed, `min_scorers` is between 1 and the number of scorers.
- Each event key is declared once. `base_points` does not exceed `max_points`. `weight` is a finite number of at least 0.
- When the effective policy sets `merit.external_contributions_earn = false`, no event may set `external_counts = true`.

## 6. `payouts.toml`

```toml
schema = 1
enabled = true                 # false is the kill switch: nothing is prepared or paid
currency = "usd"

[schedule]
cadence = "weekly"             # manual | weekly | biweekly | monthly
day = "friday"                 # weekday name, or 1..=28 for monthly
hour_utc = 17
mode = "prepare"               # prepare | auto

[limits]
min_payout_cents = 500
max_per_payout_cents = 10000
max_per_member_per_period_cents = 50000
max_per_run_cents = 250000
hold_days = 7

[approval]
approvals_required = 2         # minimum 1
min_approver_rank = 60

[eligibility]
min_rank = 0
roles = []                     # empty = any role
require_verified_identity = true

[[rate]]
branch = "main"
base_cents = 1000
merit_multiplier = 0.01
```

Validation rules:

- `currency` is three lowercase letters.
- `schedule.hour_utc` is 0 to 23. A monthly schedule needs `day` from 1 to 28. A weekly or biweekly schedule needs a weekday name. A manual schedule ignores `day`.
- Limits are consistent: `min_payout_cents <= max_per_payout_cents <= max_per_member_per_period_cents`, and `max_per_payout_cents <= max_per_run_cents`.
- `limits.hold_days` is at least the policy's `payouts.hold_days`. The policy sets the floor; the file may only hold longer.
- `approval.approvals_required` is at least 1. A payee can never approve their own payout; this is not configurable.
- `approval.min_approver_rank` and `eligibility.min_rank` are 0 to 1000.
- Every role in `eligibility.roles` exists. When the policy sets `payouts.eligible = "positions_only"`, none of them may be a tier.
- Each rate branch is declared once, is a `payout_branches` entry of some canonical forge, and has a finite `merit_multiplier` of at least 0.

## 7. Policy fields and mode defaults

The mode selects defaults; `[policy]` overrides them field by field. Code reads only the policy fields. The mode is read in exactly one function (`policy::resolve_org_policy`), and a test scans `src/cra/` to keep it that way.

| Policy field | open | hybrid | closed |
|---|---|---|---|
| `visibility.roster` | `public` | `members` | `need_to_know` |
| `visibility.leaderboard` | `public` | `members` | `off` |
| `visibility.profile` | `public` | `members` | `self_and_chain` |
| `visibility.positions` | `public` | `members` | `need_to_know` |
| `merit.external_contributions_earn` | `true` | `public_repos_only` | `false` |
| `merit.sources_enabled` | all | all | `internal.*`, `manual.award` |
| `promotion.merit_rank_ceiling` | 30 | 30 | 0 |
| `promotion.merit_may_confer_authority` | `false` | `false` | `false` |
| `promotion.eligibility_nominations` | `true` | `true` | `false` |
| `compartments.enabled` | `false` | `true` | `true` |
| `probation.days` | 14 | 30 | 90 |
| `payouts.eligible` | `verified_members` | `verified_members` | `positions_only` |
| `payouts.hold_days` | 7 | 7 | 14 |
| `payouts.two_person_min_cents` | 50000 | 25000 | 0 |
| `grants.two_person_min_rank` | 60 | 50 | 30 |
| `grants.max_term_days` | 365 | 180 | 90 |
| `audit.visibility` | `members_summary` | `officers` | `officers` |
| `anomaly.profile_reads_per_hour` | 300 | 120 | 60 |
| `anomaly.action_on_trip` | `alert` | `alert` | `alert_and_freeze_reads` |
| `consumers.subject_ids` | `pairwise` | `pairwise` | `pairwise` |

`merit.sources_enabled` accepts `"all"` or a list of patterns. `merit.external_contributions_earn` accepts `true`, `false` or `"public_repos_only"`.

## 8. Permission catalog

Every permission is `surface.action` with a class. Authority-class permissions change who holds power or money; a tier can never hold one unless the policy allows merit to confer authority.

| Class | Permissions |
|---|---|
| basic | `profile.self`, `community.read`, `community.post`, `community.vote`, `community.events`, `community.moderate`, `leaderboard.read`, `roster.read_public`, `roster.read_members`, `merit.read_own`, `payouts.read_own`, `policy.read`, `audit.read_own`, `audit.verify` |
| sensitive | `roster.read_full`, `merit.read_all`, `payouts.read_all`, `audit.read_all`, `export.roster`, `export.audit` |
| authority | `merit.award_manual`, `merit.clawback`, `members.invite`, `members.grant`, `members.approve_grant`, `members.revoke`, `members.suspend`, `members.ban`, `payouts.approve`, `policy.propose_change`, `policy.approve_change`, `compartment.manage`, `discord.reconcile`, `consumers.manage` |

## 9. Standing and the Discord projection

`standing(snapshot, member_id, vested_merit, today)` computes:

- **Tier**: the highest-rank tier whose `min_merit` the member meets. Its rank is capped at `promotion.merit_rank_ceiling`.
- **Positions**: grants that are active on `today`. A grant with `expires = D` counts through day D and not after.
- **Rank**: the highest active position rank, or the capped tier rank, whichever is higher.
- **Permissions**: tier permissions (authority-class ones are dropped unless the policy allows them, even if a bad snapshot slipped through) plus the permissions of every active position.
- **Titles**: position titles from highest rank down, then the tier title.
- **Discord roles**: the `discord_role` of each active position and of the tier.
- A suspended member keeps only `profile.self`, rank 0 and no Discord roles. A removed member keeps nothing.

`discord_projection(snapshot, merits, today)` returns every `(discord_user_id, discord_role_id)` pair that should exist for members with a Discord identity, and the managed roles ordered from highest rank to lowest, so the job titles form the Discord role hierarchy.

## 10. The change plan

`plan_change(active, new)` lists every change: mode, effective policy fields, other `org.toml` values, roles added, removed and changed (field by field), members added and removed, grants added, removed and changed, status changes, identity and name changes, and `merit.toml` and `payouts.toml` changes. Reordering entries is not a change.

It also computes the impact: members who gain or lose each position Discord role, and tiers whose rank would exceed the new merit rank ceiling. Tier Discord roles depend on merit, which the plan does not see; they are recomputed at reconciliation.

`ChangePlan::render_markdown()` is the preview shown on a pull request before merge, and before any mode switch. It ends with a table of the authority each change needs. The preview also works on a snapshot that parses but does not validate, so an author can see a mode switch's impact and the errors to fix in one place.

## 11. Who may change what

Every change is authorized against the ACTIVE snapshot, never the new one. A change cannot authorize itself.

**Who is acting.** The actor is the forge-authenticated identity that merged or pushed. It is mapped to a member through the active snapshot's identities. An actor who is not a member of the active snapshot, and is not an owner, is refused. An actor whose entry is suspended or removed is refused. Approvers are resolved the same way. Rank and permissions come from the active snapshot on the date of the change: an expired grant confers nothing.

**Owners.** The owner list comes from engine configuration. An entry is either a member id (`mckale`) or a forge identity (`github:Simbuilder`). Owners have rank 1000 and every permission. A member who is an owner is protected at rank 1000, so nobody can change an owner's entry from the repository.

**Strict inequality.** Every rank check is "strictly above". Equal rank is never enough.

| Change | The actor needs | Rank strictly above |
|---|---|---|
| Add or change a grant of role R | `members.grant` | rank of R (old or new, the higher) |
| Remove a grant of role R from member M | `members.revoke` | the higher of rank of R and rank of M |
| Create, delete or change role R | `policy.propose_change` | the higher of R's old and new rank |
| Change member M's status, or remove M's entry | `members.suspend` | rank of M |
| Add a member entry M | `members.invite` | rank of M |
| Change member M's identities or name | `members.grant` | rank of M |
| Mode, policy, other `org.toml`, `merit.toml`, `payouts.toml` | `policy.propose_change` | the higher of the old and new `grants.two_person_min_rank` |

The rank of a member M is the highest rank of any grant M holds in either snapshot (expired or not), and never below the merit rank ceiling, because the plan cannot see merit. An owner is 1000.

**Nobody changes their own** grants, status, identities, name or member entry.

**Two-person changes.** These need two people:

- every mode, policy, other `org.toml`, `merit.toml` and `payouts.toml` change;
- every role, grant, status, member entry or identity change whose rank is at or above `grants.two_person_min_rank`.

The threshold used is the lower of the old and new values, so a commit cannot raise the threshold to escape its own check.

A two-person change must arrive through a merged pull or merge request with a known author and an approving review from a second member who:

- is not the author;
- is not the actor who merged;
- is not affected by the change (the grantee, the member whose status or identity changes, or a holder of the role being changed);
- is active in the active snapshot;
- is strictly above the same rank the actor must be above;
- holds `members.approve_grant` (member changes) or `policy.approve_change` (role and org-level changes).

A direct push carries no approver, even if approvers are supplied, and fails every two-person change.

**Bootstrap.** With no active snapshot there is nobody to resolve, so only an owner listed in the `provider:login` form may load the first snapshot.

Every violation is returned, not just the first.

## 12. Walkthrough: Voltec Industries

`examples/cra/voltec-industries/.dsync/` is a hybrid organization. All names except McKale Olson, every repository path and every Discord id are placeholders.

1. **Mode.** `org.toml` sets `mode = "hybrid"`. The effective policy is the hybrid column of section 7: the merit rank ceiling is 30, two-person changes start at rank 50, grants at or above 50 expire within 180 days, compartments are on, and payouts hold for 7 days.
2. **Forges.** One public GitHub repository, where external contributors earn merit, and one private GitLab repository on `gitlab.com`, for staff. Both are canonical and pay out on `main`.
3. **Roles.** Three earned tiers (Newcomer 0, Contributor 10, Core Contributor 30; none above the ceiling and none with an authority permission) and four job titles: Engineer 20, Senior Engineer 40, Head of Cell Engineering 60 (one holder, 180-day term, compartment `cell-eng`) and Director 70.
4. **Members.** Gus Example is Director until 2027-04-30. Hana Example, a GitLab-only member, is Head of Cell Engineering until 2027-05-01. McKale Olson is an Engineer. Ivo Example is a Senior Engineer.
5. **Portal.** The `voltec-portal` consumer reads `community.*` only. DSync standing never reaches Voltec's manufacturer data, messages, user management, roles or super admin, and Voltec enforces the same allowlist in its own code.
6. **Bootstrap.** The owner, configured as `github:example-founder`, pushes the first snapshot. Nobody else can.
7. **A routine change.** Hana adds a new GitLab-only engineer and grants Engineer (20). She holds `members.invite` and `members.grant`, and 60 is above 20, so a direct push is enough.
8. **A refused change.** Hana tries to appoint a Director (70). 60 is not above 70, so it is refused even with the owner's approval.
9. **A two-person change.** Gus renews Hana's Head of Cell Engineering grant. Rank 60 is at or above the threshold 50, so it needs a pull request, and an approver other than Gus and Hana who is above 60 and holds `members.approve_grant`: the owner, or a second Director.

The same person can stand differently in each organization: McKale Olson is Lead Maintainer (80) in the Eustress Engine example, Director (70) in the Weave Solutions example, and Engineer (20) in the Voltec Industries example.

## 13. Choices this implementation made

Where the contract left a point open, the code chose the following.

- **Permissions as well as rank.** The contract's change rules are rank rules. The hierarchy document's grant rule (4.5), which the contract keeps, also requires the matching permission. The code requires both.
- **Removing a grant** needs rank above both the role and the member (contract: the role; hierarchy doc: the member). The stricter of the two applies.
- **Status changes and member entries** are two-person when the member's rank is at or above the threshold. The contract lists status changes as one-person; a suspension removes everything a revocation would, so it gets the same protection.
- **Every other `org.toml` change** (forges, consumers, Discord, compartments, name) is two-person, like mode and policy. Forges decide what pays and consumers decide what data leaves.
- **Org-level changes** need rank above the higher of the old and new `grants.two_person_min_rank`.
- **The approver may not be the actor** who merged, as well as not the author.
- **Owners** may be listed as member ids or as `provider:login`. Only the second form can bootstrap.
- **Authorization does not see merit**, so a member's protected rank never drops below the merit ceiling, and an actor's rank comes from positions only.
- **Compartments** are declared in `org.toml` `[[compartment]]`, which the contract did not define.
- **`limits.hold_days`** must be at least the policy's `payouts.hold_days`.
- **Rate branches** must be payout branches of a canonical forge.
- **Probation** is not applied: the CRA does not record when a member joined or was granted a position.
