# Apache DSync: Central Rank Authority platform contract

Status: build contract, 2026-11-12. This is the interface the implementation workers build against. It records McKale's direction of 2026-11-12 and extends `docs/HIERARCHY_AND_THREAT_MODEL.md` (branch `claude/priceless-merkle-ce50c8`). Where the two differ, section 9 says which wins.

## 1. Premise

DSync is a management hierarchy tool for open-source and closed-source organizations: role-based access control plus payroll, driven by one hierarchy.

- **The Central Rank Authority (CRA) is the organization's repository.** The administration defines roles, job titles, ranks, permissions, members, merit rules and payout policy in files under `.dsync/` in a Git repository hosted on GitHub or GitLab. A change to the hierarchy is a commit, reviewed like code.
- **DSync is the engine.** It verifies and loads the CRA, ingests forge events from GitHub and GitLab, scores work against a fixed rubric, keeps the merit ledger, computes each member's standing, and runs payouts.
- **Everything else is a projection.** The Discord bot lives in the server and applies roles from the CRA. Portals (Eustress Engine, Weave Solutions, Voltec Industries) read standing through a signed API or their own SSO. Discord is never the source of truth: a role edited by hand in Discord is drift, and drift is reverted and reported.
- **Merit tracks who moves the project forward.** Merit comes from events that land improvements (merged work, reviews, fixes, releases), weighted by a rubric score, not from activity volume. The hardest worker is visible and can be paid.
- **Connectors are how it is easy.** Discord, GitHub and GitLab link through OAuth. A portal links through a consumer key. Nothing else is needed to start.

## 2. Components

| Component | Role | Authority |
|---|---|---|
| CRA repository (`.dsync/`) | Roles, titles, ranks, permissions, members, appointments, merit rules, payout policy | Source of record for the hierarchy |
| DSync engine (this service) | Verifies and loads the CRA; merit ledger; rubric scores; standing; payouts; audit | Source of record for earned state (merit, payouts, audit) |
| Forge connectors (GitHub, GitLab) | Signed webhooks in; file reads at a commit SHA | Input only |
| Discord bot | Applies and reconciles roles | Projection |
| Portals / consumers | Read standing with scoped, signed requests | Projection |
| REST API `/api/v1` and MCP server `/mcp` | Scoped tokens; merit scoring; payouts; hierarchy reads | Guarded entry points |

Owners (rank 1000) are engine configuration (`DSYNC_OWNER_MEMBER_IDS`), never CRA content. A compromised repository cannot mint an owner.

## 3. CRA file formats (schema 1, TOML)

All files live in `.dsync/` on the CRA repository's default branch. Every file starts with `schema = 1`. Unknown keys are an error (fail closed).

### 3.1 `org.toml`

```toml
schema = 1

[org]
slug = "eustress-engine"        # [a-z0-9-], unique
name = "Eustress Engine"
mode = "open"                   # open | closed | hybrid
template = "open-source"        # informational

[policy]                        # optional overrides of the mode defaults (hierarchy doc 3.2)
"payouts.hold_days" = 10

[discord]
guild_id = "123456789012345678"
alert_channel_id = "123456789012345679"

[[forge]]                       # repositories whose events count
provider = "github"             # github | gitlab
host = "github.com"             # optional; a self-managed GitLab host goes here
repo = "EustressEngine/eustress"
visibility = "public"           # public | private
canonical = true                # a mirror sets false and earns nothing
payout_branches = ["main"]

[[consumer]]                    # portals allowed to read standing
key = "eustress-site"
label = "Eustress Engine website"
scopes = ["community.*", "profile.*"]
fields = ["tier", "positions.title", "merit.vested", "permissions"]
```

### 3.2 `roles.toml`

```toml
schema = 1

[[role]]
key = "contributor"             # [a-z0-9-], unique in org
title = "Contributor"           # the job title shown in Discord and portals
kind = "tier"                   # tier (earned from merit) | position (appointed)
rank = 10                       # 0..=999. 1000 is owners only.
permissions = ["community.post", "community.vote"]
discord_role = "123456789012345680"   # optional snowflake
min_merit = 50                  # tier only, required

[[role]]
key = "maintainer"
title = "Maintainer"
kind = "position"
rank = 60
permissions = ["members.grant", "members.revoke", "payouts.read_all"]
max_holders = 5                 # position only, optional
term_days = 365                 # position only, optional
merit_eligibility = 1000        # position only, optional: files a nomination
compartments = []               # optional
```

Rules: keys unique; ranks 0..=999; a tier needs `min_merit` and may not hold an authority-class permission unless the effective policy sets `promotion.merit_may_confer_authority`; a tier's rank may not exceed `promotion.merit_rank_ceiling`; `discord_role` unique across roles; permissions must be in the catalog (hierarchy doc 4.4) or declared in `org.toml` `permissions_extra`.

### 3.3 `members.toml`

```toml
schema = 1

[[member]]
id = "mckale"                   # stable handle, [a-z0-9-], unique in org
name = "McKale Olson"
status = "active"               # active | suspended | removed

[member.identities]             # attested by the administration in the CRA
github = "Simbuilder"
gitlab = "simbuilder"
discord = "111111111111111111"

[[member.grant]]                # appointed positions only; tiers are computed
role = "maintainer"
granted_by = "founder-1"        # member id
expires = "2027-11-01"          # required when role rank >= grants.two_person_min_rank
```

Rules: an identity value belongs to at most one member; `grant.role` must be a `position`; `granted_by` must exist; no member may be `granted_by` themselves; expiry rules follow the effective policy.

An identity listed here is proven by the administration. A contributor who is not in `members.toml` (the open-source case) earns merit under their forge login and links Discord by OAuth proof, never by typing a username.

### 3.4 `merit.toml`

```toml
schema = 1
rubric = "dsync-rubric-v1"      # the engine refuses an id it does not know
scorers = ["agent"]             # agent | coderabbit. Empty = base points only.
min_scorers = 1
score_window_days = 7           # a first score arriving this long after merge still counts

[[event]]
key = "pr_merged"               # pr_merged | review_approved | issue_closed | release_published | docs_merged | manual_award
base_points = 10                # awarded with no score
max_points = 60                 # awarded at a perfect rubric score
weight = 1.0
daily_cap = 150
vesting_days = 14
external_counts = true          # do non-members earn it
```

### 3.5 `payouts.toml`

```toml
schema = 1
enabled = true                  # false is the kill switch: nothing is prepared or paid
currency = "usd"

[schedule]
cadence = "weekly"              # manual | weekly | biweekly | monthly
day = "friday"                  # weekday name, or 1..=28 for monthly
hour_utc = 17
mode = "prepare"                # prepare: build a run and wait. auto: pay rows that are already fully approved.

[limits]
min_payout_cents = 500
max_per_payout_cents = 10000
max_per_member_per_period_cents = 50000
max_per_run_cents = 250000
hold_days = 7                   # a ledger row cannot be paid before this many days after merge

[approval]
approvals_required = 2          # distinct approvers per ledger row. Minimum 1.
min_approver_rank = 60
# A payee can never approve their own payout. This is not configurable.

[eligibility]
min_rank = 0
roles = []                      # empty = any role
require_verified_identity = true

[[rate]]                        # how merit converts to money, per branch
branch = "main"
base_cents = 1000
merit_multiplier = 0.01         # amount = base * (1 + merit_points * multiplier), then limits
```

## 4. Loading the CRA and who may change it

On a verified push webhook to the CRA repository's default branch that touches `.dsync/`:

1. Fetch the five files at the pushed commit SHA through the forge API (never from the webhook body).
2. Parse and validate (section 3 rules). Any error rejects the snapshot.
3. Diff against the active snapshot: roles added, removed or changed; grants added or removed; member status changes; policy, merit and payout changes.
4. **Authorize the change** against the ACTIVE snapshot, never the new one (a change cannot authorize itself):
   - The actor is the forge-authenticated identity that merged or pushed, mapped to a member through the active snapshot's identities. An unknown actor is refused. Owners are resolved from engine configuration.
   - Strict inequality: adding or removing a grant of role R needs `rank(actor) > rank(R)`. Creating, deleting or changing a role with rank R (old or new, whichever is higher) needs `rank(actor) > R`. Changing a member's status needs `rank(actor) > rank(member)`. Nobody changes their own grants or status.
   - Two-person: changes to `org.toml` mode or policy, `merit.toml`, `payouts.toml`, any role at or above `grants.two_person_min_rank`, and any grant of such a role must arrive through a merged pull or merge request with an approving review from a second member who is not the author, not a beneficiary, and outranks the affected role under the same strict rule. Direct pushes carry no approver and fail this check.
5. If authorized: store the snapshot (commit SHA, content hash, actor, approvers), make it active, write the audit entry, queue Discord reconciliation and consumer cache invalidation. If not: keep the active snapshot, record every violation, alert.

The mode switch preview McKale asked for is the diff from step 3, shown on the pull request before merge by the same pure function (`cra::plan_change`).

Bootstrap: the first snapshot for an org is accepted only when the actor is an owner from engine configuration.

## 5. Forge events (GitHub and GitLab)

One normalized event type feeds merit and payouts:

```
ForgeEvent { provider, host, repo, kind, actor_login, subject_id, subject_number, title, merge_sha, base_branch, delivery_id }
kind: PrOpened | PrMerged | ReviewApproved | IssueClosed | ReleasePublished | Push
```

- GitHub: `POST /github/webhook`, `X-Hub-Signature-256` HMAC over the raw body (exists today), `X-GitHub-Delivery` for dedupe.
- GitLab: `POST /gitlab/webhook`, `X-Gitlab-Token` compared in constant time against the configured secret, `X-Gitlab-Event-UUID` (fallback `Idempotency-Key`) for dedupe. Merge request hooks map to `PrOpened` and `PrMerged`; an `approved` action maps to `ReviewApproved`.
- GitHub dedupe keys keep their exact existing form, so a redelivery of an event processed before this change is still recognized. Every GitLab key is prefixed `gitlab:<host>:`, so a GitLab id can never collide with a GitHub id.
- A repository not listed in `org.toml` `[[forge]]`, or listed with `canonical = false`, earns nothing and creates no payout row. This is what stops a mirrored repository from paying twice for one merge. Until CRA loading lands (round 2) the list comes from `DSYNC_FORGE_REPOS`; with it unset, GitHub behaves as it does today and GitLab is refused.
- Contributors are keyed by `(provider, host, login)`. The legacy `contributors.github_username` column stays; a GitLab-only contributor stores `gitlab:<login>` there, a value no GitHub login can equal.

### 5.1 Many repositories, one score

An organization lists any number of repositories across GitHub and GitLab (`[[forge]]` in `org.toml`). Each repository tracks its own XP.

- Every merit entry records where it was earned: `(provider, host, repo)`. Entries with no repository (legacy rows, manual awards) fall in an `unattributed` bucket.
- One person can hold several forge identities (a GitHub login and a GitLab login). All of them map to one contributor, so their XP adds up.
- The leaderboard score is the sum of a person's XP across every repository. Each row drills down into the per-repository amounts, and the per-repository amounts always add up to the total.
- A repository marked `visibility = "private"` is never named on a public response. Its XP still counts toward the total and appears in the drilldown as one combined `private repositories` line. A caller with `merit.read` sees the names.
- A per-repository leaderboard (`?repo=`) ranks by the XP earned in that repository alone.

```
GET /api/leaderboard                      public: total per person, drilldown with private repositories combined
GET /api/v1/leaderboard?repo=&limit=      merit.read: totals or one repository, private names included
GET /api/v1/members/{id}/merit            merit.read: one person's XP by repository and by event kind
```

## 6. Merit rubric (`dsync-rubric-v1`)

CodeRabbit is optional. The engine never invents a score. With no score, an event earns `base_points`.

A score is submitted by a scorer (an agent through the MCP server or REST API; optionally CodeRabbit) against a fixed rubric. The rubric is a constant in code with a pinned SHA-256; changing it means publishing `dsync-rubric-v2`, never editing v1.

| Criterion | Weight | What level 0 to 4 means |
|---|---|---|
| `impact` | 30 | 0 no user or project effect; 1 cosmetic; 2 useful local improvement; 3 unblocks others or fixes a real defect; 4 moves a roadmap goal or fixes a critical defect |
| `correctness` | 20 | 0 broken or untested; 1 works by inspection only; 2 tested on the main path; 3 tests cover edge cases; 4 tests would catch a regression and were shown to fail without the change |
| `difficulty` | 15 | 0 trivial; 1 mechanical; 2 needs module knowledge; 3 crosses modules or needs design; 4 hard problem, novel design or deep debugging |
| `quality` | 15 | 0 hurts maintainability; 1 works but unclear; 2 clear and consistent; 3 simplifies what it touches; 4 leaves the area measurably better |
| `risk` | 10 | 0 adds a security or data-loss risk; 1 unexamined risk; 2 no new risk; 3 risk identified and contained; 4 removes an existing risk |
| `communication` | 10 | 0 no description; 1 states what; 2 states what and why; 3 reviewer can verify from the description; 4 docs and changelog updated where needed |

- `score = sum(weight * level / 4)`, 0 to 100, rounded to one decimal.
- Hard gates: `ci_passed = false` caps the score at 40. `reverted = true` sets it to 0.
- Each criterion needs an `evidence` string of at least 20 characters. A submission must echo the rubric id and hash.
- Points: `points = base_points + round((max_points - base_points) * score / 100)`. One function, one table; the duplicated tables in `merit/calculator.rs` and `github/webhook.rs` are removed.
- A scorer may not score work they authored. One submission per scorer per subject. With several scorers the median is used. Fewer than `min_scorers` submissions means base points.
- A first valid score that arrives within `score_window_days` after merge adjusts the merit once, idempotently.

## 7. API tokens, REST and MCP

- Tokens: `dsync_<id>_<secret>`, stored as SHA-256, bound to a member id, with scopes and optional expiry. Shown once at creation. Bootstrap creation uses the legacy `DSYNC_ADMIN_TOKEN`.
- Scopes: `hierarchy.read`, `merit.read`, `merit.score`, `payouts.read`, `payouts.prepare`, `payouts.approve`, `payouts.execute`, `payouts.policy`, `tokens.manage`.
- The actor on every write is the token's member. No request body names its own actor.

REST (`/api/v1`):

```
POST /tokens                          tokens.manage (or the bootstrap admin token)
GET  /payouts/policy                  payouts.read
PUT  /payouts/policy                  payouts.policy
GET  /payouts/ledger?status=          payouts.read
POST /payouts/runs                    payouts.prepare   body: {"dry_run": true}
GET  /payouts/runs/{id}               payouts.read
POST /payouts/runs/{id}/approve       payouts.approve
POST /payouts/runs/{id}/execute       payouts.execute
POST /payouts/runs/{id}/cancel        payouts.prepare
GET  /merit/rubric                    public
POST /merit/scores                    merit.score
GET  /merit/scores?subject=           merit.read
```

MCP (`POST /mcp`, JSON-RPC 2.0, same tokens and scopes): `rubric_get`, `merit_score_submit`, `merit_scores_list`, `leaderboard_get`, `member_merit_get`, `standing_get`, `payouts_policy_get`, `payouts_ledger_list`, `payouts_run_preview`, `payouts_run_create`, `payouts_run_approve`, `payouts_run_execute`, `hierarchy_get`.

## 8. Payout runs

- The ledger row per merged change stays as it is: `UNIQUE (pr_id, merge_sha)`, atomic status claim, Stripe idempotency key `dsync-ledger-<id>`. Nothing here weakens that.
- Approvals attach to ledger rows: `(ledger_id, approver)` unique. Approving a run approves each of its rows for that approver, skipping any row where the approver is the payee.
- A row is payable when: policy `enabled`; approvals from distinct members >= `approvals_required`, each with rank >= `min_approver_rank`, none of them the payee; `hold_days` passed; eligibility met; the payee is not blocked; every limit holds.
- A run is a set of ledger rows evaluated together. `dry_run` returns the plan and persists nothing. A scheduled run is created once per period (`UNIQUE (org, period_key)`). `prepare` waits for approvals and an execute call; `auto` pays the rows that are already payable.
- Executing a row goes through the existing transfer path. Limits are re-checked at execute time, inside the same claim that marks the row.
- The legacy `POST /api/admin/ledger/{id}/approve` counts as one approval by `admin-token` and pays only when policy allows a single approver. With no stored policy the behaviour is what it is today.

## 9. Relation to the hierarchy and threat model document

- Stands unchanged: the mode lever and its defaults (3), the three axes and the permission catalog (4), the strict-inequality grant rule (4.5), root accounts and identity linking rules (6), the audit log, reconciliation, compartments and break-glass (7), the consumer HMAC contract (8), the threat model (9).
- Changed by this contract: principle 1 and section 5. The hierarchy configuration is no longer edited in DSync tables through guarded API calls. It is authored in the CRA repository, and DSync's guarded-action flow for hierarchy changes becomes the change-authorization check in section 4. DSync tables hold the verified snapshot, not the editable original.
- Unanswered decisions D0 to D14 in that document still stand. Work built from this contract assumes the recommended option for D2 (an org id on every new table) and D4 (ranks 0 to 1000) and touches none of the others.

## 10. Build order

| Round | Work | State |
|---|---|---|
| 1 | Rubric, optional CodeRabbit, GitLab webhooks, forge normalization, per-repository XP and leaderboard drilldown | built and tested (merged, 2026-11-12) |
| 1 | API tokens, payout policy, runs, approvals, schedule, REST | built and tested (merged, 2026-11-12) |
| 1 | CRA parser, validator, change planner and authorizer; example CRAs for Eustress Engine, Weave Solutions, Voltec Industries | built and tested (merged, 2026-11-12) |
| 2 | MCP server; REST merit and leaderboard endpoints | in progress |
| 2 | CRA loading on push; ranks, payee identities, repositories, merit rules and payout policy from the CRA | in progress |
| 3 | Discord projection and reconciliation from the CRA; consumer sync API (HMAC) | not started |
| 4 | Root accounts, OAuth linking, sessions, audit log | not started |
