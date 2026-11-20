# MSync (MeritSync): Central Rank Authority platform contract

Status: build contract, 2026-11-12. This is the interface the implementation workers build against. It records McKale's direction of 2026-11-12 and extends `docs/HIERARCHY_AND_THREAT_MODEL.md` (branch `claude/priceless-merkle-ce50c8`). Where the two differ, section 9 says which wins.

## 1. Premise

MSync is a management hierarchy tool for open-source, source-available and closed-source organizations: role-based access control plus payroll, driven by one hierarchy.

- **The Central Rank Authority (CRA) is the organization's repository.** The administration defines roles, job titles, ranks, permissions, members, merit rules and payout policy in files under `.msync/` in a Git repository hosted on GitHub or GitLab. A change to the hierarchy is a commit, reviewed like code.
- **MSync is the engine.** It verifies and loads the CRA, ingests events from GitHub and GitLab through the Foundry API, scores work against a fixed rubric, keeps the merit ledger, computes each member's standing, and runs payouts.
- **Everything else is a projection.** The Discord bot lives in the server and applies roles from the CRA. Portals (Eustress Engine, Weave Solutions, Voltec Industries) read standing through a signed API or their own SSO. Discord is never the source of truth: a role edited by hand in Discord is drift, and drift is reverted and reported.
- **Merit tracks who moves the project forward.** Merit comes from events that land improvements (merged work, reviews, fixes, releases), weighted by a rubric score, not from activity volume. The hardest worker is visible and can be paid.
- **Connectors are how it is easy.** Discord, GitHub and GitLab link through OAuth. A portal links through a consumer key. Nothing else is needed to start.

## 2. Components

| Component | Role | Authority |
|---|---|---|
| CRA repository (`.msync/`) | Roles, titles, ranks, permissions, members, appointments, merit rules, payout policy | Source of record for the hierarchy |
| MSync engine (this service) | Verifies and loads the CRA; merit ledger; rubric scores; standing; payouts; audit | Source of record for earned state (merit, payouts, audit) |
| Foundry API (GitHub, GitLab) | Signed webhooks in; file reads at a commit SHA | Input only |
| Discord bot | Applies and reconciles roles | Projection |
| Portals / consumers | Read standing with scoped, signed requests | Projection |
| REST API `/api/v1` and MCP server `/mcp` | Scoped tokens; merit scoring; payouts; hierarchy reads | Guarded entry points |

"Foundry API" is the name for MSync's integration layer over code hosts (GitHub and GitLab today): signed webhooks in, file and review reads out. Code uses the `foundry` module and `Foundry*` types; configuration uses `[[foundry]]` and `MSYNC_FOUNDRY_REPOS`.

Owners (rank 1000) are engine configuration (`MSYNC_OWNER_MEMBER_IDS`), never CRA content. A compromised repository cannot mint an owner.

## 3. CRA file formats (schema 1, TOML)

All files live in `.msync/` on the CRA repository's default branch. Every file starts with `schema = 1`. Unknown keys are an error (fail closed).

### 3.1 `org.toml`

```toml
schema = 1

[org]
slug = "eustress-engine"        # [a-z0-9-], unique
name = "Eustress Engine"
mode = "source_available"       # open | source_available | hybrid | closed
template = "source-available"   # informational

[policy]                        # optional overrides of the mode defaults (hierarchy doc 3.2)
"payouts.hold_days" = 10

[discord]
guild_id = "123456789012345678"
alert_channel_id = "123456789012345679"

[[foundry]]                     # repositories whose events count
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

### 3.1.1 Modes

`mode` sets the policy defaults (hierarchy doc section 3.2). `[policy]` overrides any field. Four modes:

| Field | open | source_available | hybrid | closed |
|---|---|---|---|---|
| `visibility.roster` | `public` | `members` | `members` | `need_to_know` |
| `visibility.leaderboard` | `public` | `members` | `members` | `off` |
| `visibility.profile` | `public` | `members` | `members` | `self_and_chain` |
| `visibility.positions` | `public` | `members` | `members` | `need_to_know` |
| `merit.external_contributions_earn` | `true` | `true` | `public_repos_only` | `false` |
| `merit.sources_enabled` | all | all | all | `internal.*` plus `manual.award` |
| `promotion.merit_rank_ceiling` | 30 | 30 | 30 | 0 |
| `promotion.merit_may_confer_authority` | `false` | `false` | `false` | `false` |
| `promotion.eligibility_nominations` | `true` | `true` | `true` | `false` |
| `compartments.enabled` | `false` | `true` | `true` | `true` |
| `probation.days` | 14 | 30 | 30 | 90 |
| `payouts.eligible` | `verified_members` | `verified_members` | `verified_members` | `positions_only` |
| `payouts.hold_days` | 7 | 7 | 7 | 14 |
| `payouts.two_person_min_cents` | 50000 | 25000 | 25000 | 0 |
| `grants.two_person_min_rank` | 60 | 50 | 50 | 30 |
| `grants.max_term_days` | 365 | 180 | 180 | 90 |
| `audit.visibility` | `members_summary` | `officers` | `officers` | `officers` |
| `anomaly.profile_reads_per_hour` | 300 | 120 | 120 | 60 |
| `anomaly.action_on_trip` | `alert` | `alert` | `alert` | `alert_and_freeze_reads` |
| `consumers.subject_ids` | `pairwise` | `pairwise` | `pairwise` | `pairwise` |

`source_available` is for a project whose code is public but whose license is not an open-source license (Eustress Engine is the example). Anyone can read the code and outside contributions earn XP, but the roster, leaderboard and job titles are visible to members only, and merit never buys authority. It differs from `hybrid` in one default: outside contributions earn in every listed repository, not only public ones.

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
term_days = 180                 # position only, optional; at most grants.max_term_days
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
expires = "2027-05-01"          # required when role rank >= grants.two_person_min_rank
```

Rules: an identity value belongs to at most one member; `grant.role` must be a `position`; `granted_by` must exist; no member may be `granted_by` themselves; expiry rules follow the effective policy.

An identity listed here is proven by the administration. A contributor who is not in `members.toml` (the open-source case) earns merit under their GitHub or GitLab login and links Discord by OAuth proof, never by typing a username.

### 3.4 `merit.toml`

```toml
schema = 1
rubric = "msync-rubric-v1"      # the engine refuses an id it does not know
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

On a verified push webhook to the CRA repository's default branch that touches `.msync/`:

1. Fetch the five files at the pushed commit SHA through the Foundry API (never from the webhook body).
2. Parse and validate (section 3 rules). Any error rejects the snapshot.
3. Diff against the active snapshot: roles added, removed or changed; grants added or removed; member status changes; policy, merit and payout changes.
4. **Authorize the change** against the ACTIVE snapshot, never the new one (a change cannot authorize itself):
   - The actor is the identity GitHub or GitLab authenticated that merged or pushed, mapped to a member through the active snapshot's identities. An unknown actor is refused. Owners are resolved from engine configuration.
   - Strict inequality: adding or removing a grant of role R needs `rank(actor) > rank(R)`. Creating, deleting or changing a role with rank R (old or new, whichever is higher) needs `rank(actor) > R`. Changing a member's status needs `rank(actor) > rank(member)`. Nobody changes their own grants or status.
   - Two-person: changes to `org.toml` mode or policy, `merit.toml`, `payouts.toml`, any role at or above `grants.two_person_min_rank`, and any grant of such a role must arrive through a merged pull or merge request with an approving review from a second member who is not the author, not a beneficiary, and outranks the affected role under the same strict rule. Direct pushes carry no approver and fail this check.
5. If authorized: store the snapshot (commit SHA, content hash, actor, approvers), make it active, write the audit entry, queue Discord reconciliation and consumer cache invalidation. If not: keep the active snapshot, record every violation, alert.

The mode switch preview McKale asked for is the diff from step 3, shown on the pull request before merge by the same pure function (`cra::plan_change`).

Bootstrap: the first snapshot for an org is accepted only when the actor is an owner from engine configuration.

## 5. Foundry API events (GitHub and GitLab)

One normalized event type feeds merit and payouts:

```
FoundryEvent { provider, host, repo, kind, actor_login, subject_id, subject_number, title, merge_sha, base_branch, delivery_id }
kind: PrOpened | PrMerged | ReviewApproved | IssueClosed | ReleasePublished | Push
```

- GitHub: `POST /github/webhook`, `X-Hub-Signature-256` HMAC over the raw body (exists today), `X-GitHub-Delivery` for dedupe.
- GitLab: `POST /gitlab/webhook`, `X-Gitlab-Token` compared in constant time against the configured secret, `X-Gitlab-Event-UUID` (fallback `Idempotency-Key`) for dedupe. Merge request hooks map to `PrOpened` and `PrMerged`; an `approved` action maps to `ReviewApproved`.
- GitHub dedupe keys keep their exact existing form, so a redelivery of an event processed before this change is still recognized. Every GitLab key is prefixed `gitlab:<host>:`, so a GitLab id can never collide with a GitHub id.
- A repository not listed in `org.toml` `[[foundry]]`, or listed with `canonical = false`, earns nothing and creates no payout row. This is what stops a mirrored repository from paying twice for one merge. Until CRA loading lands (round 2) the list comes from `MSYNC_FOUNDRY_REPOS`; with it unset, GitHub behaves as it does today and GitLab is refused.
- Contributors are keyed by `(provider, host, login)`. The legacy `contributors.github_username` column stays; a GitLab-only contributor stores `gitlab:<login>` there, a value no GitHub login can equal.

### 5.1 Many repositories, one score

An organization lists any number of repositories across GitHub and GitLab (`[[foundry]]` in `org.toml`). Each repository tracks its own XP.

- Every merit entry records where it was earned: `(provider, host, repo)`. Entries with no repository (legacy rows, manual awards) fall in an `unattributed` bucket.
- One person can hold several Foundry API identities (a GitHub login and a GitLab login). All of them map to one contributor, so their XP adds up.
- The leaderboard score is the sum of a person's XP across every repository. Each row drills down into the per-repository amounts, and the per-repository amounts always add up to the total.
- A repository marked `visibility = "private"` is never named on a public response. Its XP still counts toward the total and appears in the drilldown as one combined `private repositories` line. A caller with `merit.read` sees the names.
- A per-repository leaderboard (`?repo=`) ranks by the XP earned in that repository alone.

```
GET /api/leaderboard                      public: total per person, drilldown with private repositories combined
GET /api/v1/leaderboard?repo=&limit=      merit.read: totals or one repository, private names included
GET /api/v1/members/{id}/merit            merit.read: one person's XP by repository and by event kind
```

## 6. Merit rubric (`msync-rubric-v1`)

CodeRabbit is optional. The engine never invents a score. With no score, an event earns `base_points`.

A score is submitted by a scorer (an agent through the MCP server or REST API; optionally CodeRabbit) against a fixed rubric. The rubric is a constant in code with a pinned SHA-256; changing it means publishing `msync-rubric-v2`, never editing v1.

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

- Tokens: `msync_<id>_<secret>`, stored as SHA-256, bound to a member id, with scopes and optional expiry. Shown once at creation. Bootstrap creation uses the legacy `MSYNC_ADMIN_TOKEN`.
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
- Changed by this contract: principle 1 and section 5. The hierarchy configuration is no longer edited in MSync tables through guarded API calls. It is authored in the CRA repository, and MSync's guarded-action flow for hierarchy changes becomes the change-authorization check in section 4. MSync tables hold the verified snapshot, not the editable original.
- Unanswered decisions D0 to D14 in that document still stand. Work built from this contract assumes the recommended option for D2 (an org id on every new table) and D4 (ranks 0 to 1000) and touches none of the others.

## 10. Build order

| Round | Work | State |
|---|---|---|
| 1 | Rubric, optional CodeRabbit, GitLab webhooks, Foundry API normalization, per-repository XP and leaderboard drilldown | built and tested (merged, 2026-11-12) |
| 1 | API tokens, payout policy, runs, approvals, schedule, REST | built and tested (merged, 2026-11-12) |
| 1 | CRA parser, validator, change planner and authorizer; example CRAs for Eustress Engine, Weave Solutions, Voltec Industries | built and tested (merged, 2026-11-12) |
| 2 | MCP server; REST merit and leaderboard endpoints | built and tested (merged, 2026-11-13) |
| 2 | CRA loading on push; ranks, payee identities, repositories, merit rules and payout policy from the CRA | built and tested (merged, 2026-11-13) |
| 3 | Discord projection and reconciliation from the CRA; consumer sync API (HMAC) | not started |
| 4 | Root accounts, OAuth linking, sessions, audit log | not started |

## 11. Payroll control (owner direction, 2026-11-19)

The business owner controls what gets paid, to whom, how much and when. Merit-based payouts (section 8) become one source among several. Every source ends in the same ledger, so the single transfer path, the approvals, the limits, the hold and the kill switch apply to all of them.

### 11.1 Where it lives

Compensation is confidential, so it is engine data behind scoped tokens, not CRA repository content. The CRA still decides who may act: ranks, approver rules and payee identities come from it.

### 11.2 Payees

A payee is a person or company that can be paid.

- Fields: id, display name, legal name, kind (`individual` | `company`), email, optional CRA member id, payee keys (the GitHub or GitLab logins they are paid under), Stripe Connect account id and its payout-enabled state, tax form status (`none` | `requested` | `on_file`), status (`active` | `paused` | `closed`), notes.
- MSync never stores a bank account or card number. The payout account is the payee's Stripe Connect account; the business's funding source is its own Stripe balance.
- A payee that is `paused` or `closed`, or whose Stripe account cannot receive transfers, is never paid.

### 11.3 Pay orders

A pay order is an instruction to pay. It has an amount rule and a schedule.

Amount rules:

| Rule | Meaning |
|---|---|
| `fixed` | A set amount in cents to one payee (salary, stipend, retainer, bonus). |
| `pool_percent` | A pool amount for the period, split by fixed percentages across named payees. The percentages must sum to at most 100. Any remainder is not paid. |
| `pool_by_xp` | A pool amount for the period, split in proportion to XP earned in that period, optionally limited to listed repositories, roles or payees, with an optional minimum XP to qualify and a maximum share per payee. |
| `percent_of_amount` | A percentage of a stated base amount (a revenue figure, a contract value) to one payee. The base is entered with the order or with each occurrence. |

Money is integer cents. Percentages are basis points (1 = 0.01%). Splits round down per payee; the leftover cents stay unpaid and are reported, never given to an arbitrary payee.

Schedules:

- `once`: one occurrence on a date.
- `repeat`: `weekly`, `biweekly`, `monthly` or `quarterly`, with a start date, and an optional end date or occurrence count.
- An order can be `draft`, `active`, `paused`, `ended` or `cancelled`. Pausing skips occurrences; it does not queue them up.

Each occurrence is materialized exactly once (`UNIQUE (order_id, period_key)`) into one ledger row per payee, so a restart or a double tick can never pay twice.

### 11.4 Invoices

A payee, or staff on their behalf, submits an invoice: number (unique per payee), issue date, due date, currency, line items (description, quantity, unit amount in cents), optional tax amount, total, memo, and an optional attachment reference.

- States: `submitted`, `approved`, `rejected`, `scheduled`, `paid`, `void`.
- The total must equal the sum of the lines plus tax, or the invoice is refused.
- Approval follows the same rule as any payout: distinct approvers at or above the minimum rank, never the payee. An approved invoice becomes one ledger row, payable on its due date or at once.
- A duplicate invoice number for the same payee is refused. A paid invoice can never be paid again.
- Every state change records who did it and when.

### 11.5 Ledger sources

The ledger gains a source for every row: `merit` (a merged change, today's rows), `pay_order` or `invoice`, with a reference to the order occurrence or the invoice, and the payee id. Existing rows are `merit`. The existing unique key and transfer idempotency key are unchanged.

Creating or changing a pay order, and approving an invoice, are two-person actions when the policy's `approvals_required` is above one: the creator is never an approver of their own order, and an order pays nothing until approved. A payee can never create, approve or edit an order or an invoice that pays themselves, except submitting their own invoice.

### 11.6 Scopes, REST and MCP

New scopes: `payees.read`, `payees.manage`, `payorders.read`, `payorders.manage`, `payorders.approve`, `invoices.submit`, `invoices.read`, `invoices.approve`.

```
GET/POST   /api/v1/payees            GET/PATCH /api/v1/payees/{id}
GET/POST   /api/v1/payorders         GET/PATCH /api/v1/payorders/{id}
POST       /api/v1/payorders/{id}/approve | pause | resume | cancel
GET        /api/v1/payorders/{id}/preview?periods=3     what the next occurrences would pay, nothing persisted
GET/POST   /api/v1/invoices          GET /api/v1/invoices/{id}
POST       /api/v1/invoices/{id}/approve | reject | void
GET        /api/v1/payouts/statement?payee=&from=&to=   everything paid or pending to a payee, by source
```

MCP tools mirror these: `payees_list`, `payee_get`, `payorders_list`, `payorder_preview`, `payorder_create`, `payorder_approve`, `payorder_pause`, `invoices_list`, `invoice_submit`, `invoice_approve`, `payout_statement`. Tools that move toward money (`payorder_approve`, `invoice_approve`) follow the same rules as REST; execution still goes through payout runs and the MCP execute guard.

## 12. MSync: the rebrand and two chat platforms (owner direction, 2026-11-19)

DSync is now **MSync**, short for **MeritSync**. Earlier sections have been rewritten to the new names; the table below keeps the old names as the record of what changed, and the rules below win.

### 12.1 Names

| Was | Is |
|---|---|
| Product name DSync, Apache DSync | MSync (MeritSync). The license is still Apache 2.0. |
| Crate and binary `dsync` | `msync` |
| Repository config directory `.dsync/` | `.msync/` |
| Environment variables `DSYNC_*` | `MSYNC_*`. A `DSYNC_*` variable is still read when its `MSYNC_*` twin is unset, with a startup warning, because a running deployment may have them set. |
| API token prefix `dsync_` | `msync_` (no token has been issued in a release) |
| Rubric id `dsync-rubric-v1` | `msync-rubric-v1`, with a new pinned hash (no score has been stored in a release) |
| Docs, comments, log and error text, MCP server name | MSync |

Four names do NOT change, each for a stated reason:

- The Stripe idempotency key `dsync-ledger-<id>`. A payout approved before the rename and retried after it must present the same key, or Stripe could pay twice.
- The Stripe account-creation idempotency key `dsync-account-<login>`, for the same reason: a retried account creation must not open a second account.
- The default database file `sqlite://dsync.db`. Changing the default would start an existing deployment on an empty database.
- The existing webhook dedupe keys.

### 12.2 Chat platforms: Discord, Slack, or both

An organization connects Discord, Slack, or both. Neither is the source of truth; both are projections of the CRA.

- `org.toml` has optional `[discord]` (guild id, alert channel) and optional `[slack]` (team id, alert channel). At least one must be present.
- A role may carry `discord_role` (a role id) and `slack_usergroup` (a user group id). Slack has no roles: a job title is projected as membership in a Slack user group.
- A member's identities may include `discord` (a user id) and `slack` (a user id).
- One command set, defined once, runs on both: `link`, `stats`, `leaderboard`, `sync`, `payouts`, `payout-setup`. Each command is a platform-neutral function that takes who asked and returns a reply; a Discord adapter and a Slack adapter translate.
- Slack: slash commands arrive at `POST /slack/commands`. Every request is verified with the Slack signing secret (`X-Slack-Signature` `v0` HMAC-SHA256 over `v0:<timestamp>:<body>`, constant-time compare, timestamp within 5 minutes). A reply that was private on Discord is ephemeral on Slack.
- The same person on both platforms is one contributor: their Discord and Slack ids both map to it.
- `link` on both platforms refuses to bind a GitHub or GitLab login that is already bound to a different chat user, and refuses to change an existing binding. With a CRA active, identities come from `members.toml` and `link` only reports them.
- Admin-only commands use the CRA rank or the platform's admin flag, and fail closed when neither can be read.
- MSync ships both bots: the Discord bot as today, and a Slack app manifest (`slack/manifest.yaml`) with the slash commands, scopes and request URL to install.

### 12.3 MSync as an OAuth 2.0 and OpenID Connect provider (designed, not built)

So that a platform such as Eustress Engine can sign people in with MSync and host its groups on it:

- MSync becomes an authorization server: authorization code flow with PKCE, an OpenID Connect discovery document, a JWKS endpoint, ID tokens signed with a rotating key, refresh tokens, and token revocation.
- Each platform is a registered client (client id, redirect URIs, allowed scopes), created by an owner. This replaces the per-portal `[[consumer]]` shared secret for sign-in; the signed sync API stays for server-to-server reads.
- Standing travels as claims and scopes: `openid`, `profile`, `groups` (the member's roles and job titles in that organization), `rank`, `merit`. A client receives only the claims its registration allows, and a pairwise subject id, so two platforms cannot correlate people.
- Groups: a group is a CRA role, or a named set of roles. A platform reads membership from the `groups` claim at sign-in, from `GET /oauth/userinfo`, and, to stay current between sign-ins, from a SCIM 2.0 feed or a signed webhook when a snapshot changes.
- Sign-in itself is the root account with linked identities (GitHub, GitLab, Discord, Slack, Google, email link). This depends on root accounts and sessions, which are not built.
- A suspended member gets no token, and existing tokens are revoked when a snapshot removes them.

## 13. The 90% line (owner direction, 2026-11-20)

The goal is a system the owner can run and try end to end. `docs/HIERARCHY_AND_THREAT_MODEL.md` is now in this branch; it was written before the CRA and the rename, so where it says DSync read MSync, and where it conflicts with this contract, this contract wins.

### 13.1 Audit log

Hierarchy doc 7.2. An append-only, hash-chained table: sequence, time, actor, action, target, a JSON detail, the previous entry's hash and this entry's hash (SHA-256 over the canonical fields and the previous hash). Database triggers refuse UPDATE and DELETE. A verify function walks the chain and names the first broken entry. Written for: CRA snapshot activated or rejected, token created or revoked, payout policy changed, payee created or changed (always when the Stripe account changes), pay order created, edited, approved, paused, cancelled, invoice transitions, ledger approvals, every transfer attempt and its outcome, chat link attempts that were refused, role sync changes and drift. `GET /api/v1/audit` (`audit.read`) and `GET /api/v1/audit/verify`. MCP: `audit_list`, `audit_verify`.

### 13.2 Portal sync API

Hierarchy doc section 8, with consumers taken from the CRA `[[consumer]]` list. `GET /api/v1/members/by-discord/{id}`, `by-slack/{team}/{id}` and `/api/v1/members/{subject}`, each with `?scopes=`. HMAC with timestamp and nonce exactly as specified there, headers named `X-MSync-*`. Secrets from `MSYNC_CONSUMER_SECRET_<KEY>_<KEYID>`. Scope and field allowlists per consumer, pairwise subject ids, one answer for every kind of "not found", suspended members return an empty permission list, per-consumer rate limit, every lookup audited.

### 13.3 Role sync and drift

Hierarchy doc 7.3, for both chat platforms. On snapshot activation, on member join, and on a timer, compute the projection and apply the difference. A managed role or user group changed by hand is drift: revert it, record it, alert the platform's alert channel, and stop reverting the same pair after three reverts in an hour (alert instead). Only managed roles and groups are touched. A role mapped to an administrator-class Discord permission is refused. Slack replies that may exceed three seconds answer at once and finish through `response_url`.

### 13.4 Accounts, sign-in and the OAuth provider

Hierarchy doc section 6 and contract 12.3. Root accounts with identities (`github`, `gitlab`, `discord`, `slack`, `google`, `email`), sessions, the linking rules ported from the reference (`decide`, `can_unlink`), OAuth sign-in with GitHub, GitLab, Discord and Slack (each enabled only when its client id and secret are set), and one-time email links behind a mail-sender trait. A completed OAuth sign-in marks that chat or code-host identity verified, which is what `payout-setup` and payout eligibility accept besides a CRA listing. Then MSync as an authorization server: registered clients, authorization code with PKCE, OpenID Connect discovery, JWKS, ID tokens, userinfo with a `groups` claim, pairwise subject ids, refresh and revocation.

### 13.5 Running it

One command starts the service with only a database: no Discord, no Slack and no Stripe configured is a valid local mode. A local directory can stand in for the CRA repository in development (`MSYNC_CRA_LOCAL_DIR`, refused unless `MSYNC_DEV_MODE=true`, loaded as the configured owner). A quickstart, and a smoke test that drives a real running binary through signed webhooks, a score, a payout dry run, a pay order, an invoice, a Slack command and an MCP call.

### 13.6 The repository as a public template (owner direction, 2026-11-20)

There is no dashboard in the 90%. The MSync repository is public and works as a template and a kit, in the way an SDK does: a person gives it to an AI assistant such as Claude, and the assistant sets MSync up for them through the files, the REST API and the MCP server.

- **Agent instructions at the root**: `AGENTS.md` (and a `CLAUDE.md` that points to it): what MSync is in ten lines, the setup steps in order, the commands to run, how to check each step worked, what never to do (commit a secret, turn on MCP payout execution, approve anything on the owner's behalf), and which decisions belong to the human (mode, roles and ranks, who is an owner, payout amounts). Written to be followed literally.
- **Templates**: `templates/<name>/.msync/` for `open-source`, `source-available`, `hybrid`, `closed-source` and `merit-dao`, each a complete, valid, commented CRA with placeholders an assistant fills in, plus a one-page `README.md` saying who it is for.
- **A command-line front door** in the same binary: `msync init --template <name> --dir <path>` (writes `.msync/` from a template, refusing to overwrite), `msync check <dir>` (parse and validate, print every error with file and reason, exit non-zero on failure), `msync plan <old-dir> <new-dir>` (the change preview and who would have to approve it), `msync token create ...` (bootstrap the first API token), `msync doctor` (which connectors are configured, with each missing variable named), `msync serve` (the default).
- **A reviewable hierarchy**: a GitHub Actions workflow and a GitLab CI job, shipped in each template, that run `msync check` and `msync plan` on a pull or merge request that touches `.msync/` and post the preview as a comment.
- **Machine-readable surfaces**: `openapi.yaml` for `/api/v1`, generated or hand-written but tested against the registered routes so it cannot drift; an example `.mcp.json` for connecting a client to `/mcp`; and `docs/SETUP_WITH_AN_ASSISTANT.md` for the human, with the three prompts to paste.
- **Public-repository hygiene**: no secret in the tree or in history, a `SECURITY.md` with how to report a vulnerability, and `.env.example` as the only env file.
