# DSync Hierarchy, Authority and Threat Model

Status: DESIGN, for review. No migration or code in this document has been written yet.
Owner of the schema calls: McKale. Open decisions are collected in section 12.
Date: 2026-11-12.

This document defines:

1. The organization lever (open source, closed source, hybrid) as one policy object.
2. Three separate axes that are one number today: merit, position, permissions.
3. A config schema that replaces every hardcoded threshold, plus four templates.
4. A threat model for three adversaries (Spies, Traitors, Criminals), each threat mapped to a control and a test.
5. Root account SSO: one account, many identities.
6. The contract for the first consumer, Voltec.
7. The migration and build plan, with a deliberate mutation per security test.

---

## 1. Current state (verified in code, 2026-11-12)

What exists across the four repos. Every item below was read in source, not inferred.

### 1.1 ApacheDSync-Discord (Rust, Actix, sqlx/SQLite): the source of truth

- Tables: `contributors` (one row per GitHub login: `github_username UNIQUE`, `discord_user_id`, `total_merit_points`, `current_role` string, `stripe_account_id`, `stripe_payouts_enabled`), `contributions`, `merit_roles`, `merit_activities`, `payout_ledger`, `processed_webhook_events`.
- Tiers are hardcoded twice: `src/merit/tracker.rs` (`MeritTracker::new`) and the seed in `migrations/20240101000000_initial_schema.sql`. Newcomer 0, Contributor 50, Active Contributor 150, Core Contributor 500, Maintainer 1000.
- Rank is the merit number. "Maintainer" is described as "full permissions" and is reached by merit alone.
- Routes: `/github/webhook` (HMAC-SHA256 verified, constant time), `/api/contributor/stats` and `/api/leaderboard` (PUBLIC, no auth), `/stripe/webhook` (Stripe-Signature verified with a 300 s tolerance), `/api/admin/ledger*` and `/api/payouts/onboard` (one shared bearer token, `DSYNC_ADMIN_TOKEN`), `/payouts/return`, `/payouts/refresh` (HMAC-signed).
- Slash commands: `/link`, `/stats`, `/leaderboard`, `/sync`, `/payouts`, `/payout-setup`.

What is already good and MUST survive every phase below:

- Webhook dedupe: `X-GitHub-Delivery` claim plus per-effect keys (`pr_opened:`, `pr_merged:<pr>:<sha>`, `review_approved:<pr>:<login>`) in `processed_webhook_events`, released on failure. Tests: `duplicate_deliveries_do_not_double_award_merit`, `duplicate_review_approval_awards_once`, `claim_and_release`.
- Baseline before any change: `cargo test` at `9414f46` passes, 23 lib tests and 3 integration tests, 0 failed (run 2026-11-12 against a copy of `dsync-local.db` with all three migrations applied, since the sqlx `query!` macros need a schema at compile time).
- Payout ledger: `UNIQUE (pr_id, merge_sha)`, `pending_approval` only on merge, money moves only in `ledger::approve`, an atomic status claim (`UPDATE ... WHERE status = ?`), and Stripe idempotency key `dsync-ledger-<id>`. Live Stripe keys are refused unless `DSYNC_STRIPE_ALLOW_LIVE=true`. Tests in `src/payments/tests.rs`.

### 1.2 Holes found during this review

Ordered by severity. P0 means fix before anything else ships.

| # | Severity | Finding | Where |
|---|---|---|---|
| H1 | **P0, live** | `/link github:<name>` binds the caller's Discord id to ANY GitHub login with no proof, and overwrites an existing binding. The attacker then inherits the victim's merit and role. If the victim has no Stripe account yet, `/payout-setup` creates one owned by the attacker, so the victim's approved payouts go to the attacker. | `src/discord/commands.rs` `link_account`; `src/payments/onboarding.rs` |
| H2 | **P0** | Merit buys payout authority. The Website admin gate admits anyone whose `current_role` is "Core Contributor" or "Maintainer" (merit 500+), and `/admin/ledger` approves payouts through the shared `DSYNC_ADMIN_TOKEN`. Earned points become money-moving power. | Website `lib/apache_dsync_web/live_auth.ex:11`, `router.ex:43-45`, `payouts_admin.ex:50-52` |
| H3 | P0 | No separation of duties on payouts. `decided_by` is free text from the request body, and nothing stops the payee from approving their own entry. | `src/payments/routes.rs` `DecisionRequest` |
| H4 | P1 | `/sync` admin check is fail open: if `command.member` is `None` or `permissions()` errors, the check passes. The command is also a stub that syncs nothing. | `src/discord/commands.rs` `sync_roles` |
| H5 | P1 | `/api/contributor/stats` and `/api/leaderboard` are public: the roster, merit and roles of every contributor are enumerable by anyone. | `src/routes.rs` |
| H6 | P1 | Identity is keyed by GitHub **login**, which GitHub lets people rename and others re-register. A recycled login inherits merit and the payout destination. The webhook payload carries the stable numeric `user.id`; it is ignored. | `src/github/webhook.rs` `get_or_create_contributor` |
| H7 | P1 | Merit farming: every opened PR earns 5 points at once, merged or not. Self-approval earns review merit (no check that reviewer != author). No caps, no vesting. | `src/github/webhook.rs` |
| H8 | P2 | Roles never re-evaluate after merit changes. `check_role_progression` is called only by `recalculate_all_roles`, which nothing calls; `DiscordSyncService` is never constructed. Discord roles change only when a member joins the guild. Hand-edited Discord roles are never detected or reverted. | `src/merit/tracker.rs`, `src/services/discord_sync.rs`, `src/discord/bot.rs` |
| H9 | P2 | `contributions` are updated by `github_pr_number` alone. PR #7 in two repos collide. The stats query compares `status = 'Merged'` while the code stores the enum as an integer, so `merged_contributions` is always 0. | `src/github/webhook.rs`, `src/routes.rs` |
| H10 | P2 | Two merit tables disagree: `webhook.rs::calculate_merit_points` (max 50) and `MeritCalculator::calculate_pr_merit_from_score` (max 60). | `src/github/webhook.rs`, `src/merit/calculator.rs` |
| H11 | P2 | Three role ladders disagree. The bot: Newcomer, Contributor, Active Contributor, Core Contributor, Maintainer. The Website's Ecto validators: Newcomer, Contributor, Regular, Core, Maintainer. The Website's Supabase seed: Observer 0, Contributor 100, Committer 500, PMC Member 2000, PMC Chair 5000, where merit alone grants `admin` and `veto`. The Website's admin gate reads its own Postgres copy, not the bot's. | Website `admin/banned_user.ex:39-40`, `supabase/seed.sql:3-7` |
| H12 | P2 | Website calls to the bot (`bot_client.ex`) carry no auth at all, and several target routes the bot does not have (`/api/sync-roles`, `/api/stats`). Desktop calls hardcode `http://localhost:8080` with no auth. | Website `lib/apache_dsync/bot_client.ex:182`, Desktop `src/api_client.rs:29` |
| H13 | P1 | The Website's own `/api` scope has no auth pipeline: `POST /api/bot/sync-roles`, `GET /api/bot/status` and the `/api/analytics/*` routes (overview, contributors, leaderboard) are open to anyone. | Website `lib/apache_dsync_web/router.ex:66-80` |

### 1.3 Sibling repos

- **ApacheDSync-Website** (Phoenix 1.7, Ecto/Postgres, about 5,600 lines of real Elixir): Supabase OAuth with Discord and GitHub only. The token is validated by calling Supabase `/auth/v1/user`; the JWT signature is not verified locally. Cookie session holds `current_user` with `discord_id` and `github_username` from Supabase `identities[]`. Has its own role-change voting (`pending_role_changes`, `role_votes` unique per change and voter), `banned_users` (the banner must outrank the target), `role_settings` with `approval_mode` (`auto_approve`, `voting`, `admin_decision`), and multi-guild tables (`discord_guilds`, `discord_channel_configs`, `discord_role_mappings`). These are good ideas living in the wrong place: they move into DSync as guarded actions (section 7.1) and Discord projection config (section 7.3), so there is one authority. A separate session is rebuilding the Website as Rust plus Leptos SSR on Cloudflare; that rebuild is a consumer of this design (section 8.7). Hygiene note: an untracked, gitignored `.env.dev` in the Website working tree holds real-looking secrets (`DISCORD_CLIENT_SECRET`, `DISCORD_BOT_TOKEN`, Supabase keys, `SECRET_KEY_BASE`), and `erl_crash.dump` files exist in the trees; none are committed, but crash dumps can contain environment values, so delete them and rotate anything that has been shared.
- **ApacheDSync-Desktop** (Tauri 1 plus Phoenix LiveView, no DB, mostly scaffold): no auth. README.md:40 states the need: "connect both Discord and GitHub to a root account with Email options through Google or Email." README.md:36: same APIs and permissions for Web and Desktop.
- **ApacheDSync-API**: empty directory.

Conclusion: DSync-Discord is the only place with real data and real money. It becomes the single authority for accounts, rank and permissions. Website and Desktop become clients of it.

---

## 2. Design principles

1. **One authority.** Accounts, rank, positions, permissions and policy live in DSync-Discord. Discord roles, Website role strings and Voltec permissions are projections. A projection that disagrees with DSync is drift, and drift is reverted and reported.
2. **Code asks for permissions, never for roles or modes.** `can(account, "payouts.approve")`, never `role == "Maintainer"` and never `org.mode == Closed`. Role names and mode names are labels.
3. **Rank and permissions are separate axes.** Permissions say what you may do. Rank says whom you may grant to. A permission to grant never lets you grant at or above your own rank.
4. **Authority lives on the account, never on an identity.** Linking a provider adds a door. It never adds rank.
5. **Earned is not appointed.** Merit can unlock earned tiers and eligibility. Authority comes from appointment, unless the org explicitly configures otherwise, and closed mode does not by default.
6. **High-impact actions need two people.** No one approves their own action, and no one approves an action that benefits them.
7. **Fail closed.** Missing data, a missing header, an unknown permission, an expired policy: deny.
8. **Every authority change is audited** in an append-only, hash-chained log whose head is anchored outside the database.
9. **Additive migrations.** No existing table is rebuilt. Existing payout and dedupe tables are not altered.

---

## 3. The lever: organization mode as a policy object

### 3.1 Shape

An org has a `mode` (`open`, `closed`, `hybrid`) and an optional `overrides` document. The effective policy is computed once:

```
effective_policy = mode_defaults(mode) <- overrides   (field-wise; overrides win)
```

The rest of the system reads only `Policy` fields. `mode` is read in exactly one function, `policy::mode_defaults`. A unit test scans `src/` and fails if `.mode` is read anywhere else; that is what keeps this a lever and not a flag sprinkled through code.

### 3.2 Policy fields and defaults per mode

| Policy field | open | hybrid | closed | Meaning |
|---|---|---|---|---|
| `visibility.roster` | `public` | `members` | `need_to_know` | Who can list members. `need_to_know` = only members who share a compartment with you, plus your chain of command. |
| `visibility.leaderboard` | `public` | `members` | `off` | Leaderboard exposure. |
| `visibility.profile` | `public` | `members` | `self_and_chain` | Who can see one member's tier, positions, merit. |
| `visibility.positions` | `public` | `members` | `need_to_know` | Whether job titles are visible. |
| `merit.external_contributions_earn` | `true` | `public_repos_only` | `false` | Whether PRs from non-members / public repos earn merit. |
| `merit.sources_enabled` | all | all | `internal.*` plus `manual.award` | Which merit sources count. |
| `promotion.merit_rank_ceiling` | 30 | 30 | 0 | Highest rank merit alone can reach. 0 = merit confers no rank. |
| `promotion.merit_may_confer_authority` | `false` | `false` | `false` | Whether an earned tier may carry authority-class permissions. Only the DAO template sets `true`. |
| `promotion.eligibility_nominations` | `true` | `true` | `false` | Crossing a position's merit bar files a nomination for a human to approve. |
| `compartments.enabled` | `false` | `true` | `true` | Need-to-know compartments. |
| `probation.days` | 14 | 30 | 90 | New accounts and new grants are restricted for this long. |
| `payouts.eligible` | `verified_members` | `verified_members` | `positions_only` | Who can be paid. |
| `payouts.hold_days` | 7 | 7 | 14 | Days a ledger row waits before it can be approved. |
| `payouts.two_person_min_cents` | 50000 | 25000 | 0 | Above this, two approvers. 0 = always two. |
| `grants.two_person_min_rank` | 60 | 50 | 30 | Grants at or above this rank need a second approver. |
| `grants.max_term_days` | 365 | 180 | 90 | Positions at or above `two_person_min_rank` must expire. |
| `audit.visibility` | `members_summary` | `officers` | `officers` | Who can read the audit log. Everyone can always read their own entries. |
| `anomaly.profile_reads_per_hour` | 300 | 120 | 60 | Bulk read threshold before an alert. |
| `anomaly.action_on_trip` | `alert` | `alert` | `alert_and_freeze_reads` | What a tripped threshold does. |
| `consumers.subject_ids` | `pairwise` | `pairwise` | `pairwise` | Consumers see a per-consumer pseudonymous id, never the raw account id. |

### 3.3 Switching modes is a guarded action

A mode switch or override change is a `policy.change` guarded action (section 7.1):

1. **Propose** (`policy.propose_change`): the proposer submits the target mode and overrides. DSync computes and stores:
   - the field diff (old to new, per field above),
   - the impact: how many members lose roster or leaderboard visibility, how many tiers would exceed the new `merit_rank_ceiling` and lose authority, how many pending payouts change eligibility, which consumers would see fewer permissions,
   - a `payload_hash` of the proposal.
2. **Preview**: `GET /api/v1/policy/changes/{id}` shows exactly that diff and impact. Nothing changes yet.
3. **Approve** (`policy.approve_change`): a different account, rank at least `max(proposer rank, 70)`. The approver signs the same `payload_hash`; if the proposal was edited after review, approval fails.
4. **Apply**: one transaction updates the org, increments `policy_version`, writes the audit entry, and queues re-evaluation (Discord reconcile, consumer cache invalidation).
5. Proposals expire after 72 hours.

Tightening (open to closed) and loosening (closed to open) use the same flow. There is no fast path.

---

## 4. Three axes

### 4.1 Merit (earned)

Merit is a ledger of events, not a single counter.

- `merit_events`: one row per earned or removed amount. Columns: `account_id`, `source` (for example `github.pr_merged`), `source_ref` (the dedupe key, unique per source), `raw_points`, `weight`, `points` (= raw times weight, rounded), `state` (`pending`, `vested`, `clawed_back`), `vests_at`, `created_at`, `clawback_reason`.
- Balance = sum of `points` where `state = 'vested'`. `contributors.total_merit_points` stays as a cache so existing reads and the payout formula keep working.
- Merit sources are config (`merit_sources`): key, enabled, weight, daily cap per account, vesting days, whether external (non-member) activity counts, and an optional per-pair cap for reviews. Initial sources: `github.pr_merged`, `github.review_approved`, `github.issue_resolved`, `discord.helpful` (awarded by an authorized member), `manual.award` (guarded), `internal.task_closed` (closed orgs, from an internal tracker later).
- `github.pr_opened` is retired as a source. Opening a PR earns nothing; merging does. Today's 5 points per open PR is the main farming vector (H7).
- Points scale lives in one place, `merit_sources.raw_points_table`, so H10 cannot recur.

### 4.2 Position or job title (appointed)

A position is a named seat with a rank and a permission set. Examples: "Contractor", "Engineer", "Head of Cell Engineering", "Board Member", "Maintainer".

- `positions`: `org_id`, `key`, `title`, `rank` (integer 0 to 1000), `max_holders` (optional), `default_term_days`, `merit_eligibility` (optional merit bar that files a nomination), `is_system`.
- `position_grants`: `account_id`, `position_id`, `state` (`pending_approval`, `active`, `expired`, `revoked`), `granted_by`, `approved_by`, `granted_at`, `starts_at`, `expires_at`, `revoked_at`, `revoked_by`, `reason`.
- A member can hold several positions. Effective rank is the highest active one.
- Positions with rank at or above `grants.two_person_min_rank` must have `expires_at`, at most `grants.max_term_days` out. Renewal is a new grant through the same rules.

### 4.3 Earned tiers

Tiers replace the hardcoded `merit_roles` thresholds.

- `tiers`: `org_id`, `key`, `label`, `min_merit`, `rank` (must be at most `promotion.merit_rank_ceiling`), `min_account_age_days`, `requires_identities` (for example `["github"]`), `discord_role_id`.
- A member's tier is the highest tier whose conditions they meet. Tiers are computed, never granted by hand (a manual merit award is the way to adjust, and it is guarded).
- Validation refuses a tier whose `rank` exceeds the ceiling, or whose permission set contains an authority-class permission while `merit_may_confer_authority` is false. The evaluator also filters at read time, so a bad row in the DB still cannot grant authority.

### 4.4 Permissions (atomic capabilities)

Permissions are `surface.action` strings in a catalog. Every permission has a class:

- `basic`: participation (`community.post`, `leaderboard.read`, `profile.self`).
- `sensitive`: reads that enable exfiltration (`roster.read_full`, `export.roster`, `merit.read_all`, `audit.read_all`).
- `authority`: changes who holds power or money (`members.grant`, `members.approve_grant`, `members.revoke`, `members.suspend`, `members.ban`, `payouts.approve`, `policy.propose_change`, `policy.approve_change`, `merit.award_manual`, `merit.clawback`, `discord.reconcile`, `consumers.manage`).

Initial catalog:

| Surface | Actions |
|---|---|
| `profile` | `self` |
| `community` | `read`, `post`, `vote`, `events`, `moderate` |
| `leaderboard` | `read` |
| `roster` | `read_public`, `read_members`, `read_full` |
| `merit` | `read_own`, `read_all`, `award_manual`, `clawback` |
| `members` | `invite`, `grant`, `approve_grant`, `revoke`, `suspend`, `ban` |
| `payouts` | `read_own`, `read_all`, `approve` |
| `policy` | `read`, `propose_change`, `approve_change` |
| `audit` | `read_own`, `read_all`, `verify` |
| `compartment` | `manage` |
| `export` | `roster`, `audit` |
| `discord` | `reconcile` |
| `consumers` | `manage` |

Sources of permissions: tier sets, position sets, and compartment memberships (which gate data, not actions). Effective permissions:

```
effective = (tier_perms(tier) filtered by policy) ∪ (∪ position_perms(p) for active p)
          minus probation_mask (if on probation)
          minus suspension_mask (everything except profile.self, if suspended)
```

This is a pure function over loaded rows. It is the only place permissions are computed.

### 4.5 The grant rule (strict inequality)

```
can_grant(actor A, target T, position P):
  A != T                                         (no self-grant)
  A.status == active and A is not on probation
  "members.grant" in effective(A)
  P.rank < rank(A)                               (STRICT: never equal, never above)
  if P.rank >= policy.grants.two_person_min_rank:
      needs approval by B where
        B != A and B != T
        "members.approve_grant" in effective(B)
        P.rank < rank(B)
  if P.rank >= policy.grants.two_person_min_rank: expires_at is required and within max_term_days

can_revoke(actor A, grant G of target T):
  A != T
  "members.revoke" in effective(A)
  rank(T) < rank(A)                              (you cannot remove a peer or a superior)
  two-person if G.position.rank >= two_person_min_rank

rank(X) = max(active position ranks of X, min(tier rank of X, policy.merit_rank_ceiling))
```

Rank is evaluated at the moment of approval, not proposal: if A was demoted in between, the grant fails.

**Owners.** The top seat (rank 1000) is never granted through the app. Owner account ids live in configuration only (`DSYNC_OWNER_ACCOUNT_IDS`), as Weave does with `OWNER_USER_IDS`. An owner may grant anything below 1000 under the normal rules. A traitor with full app access still cannot mint an owner; doing so requires a deploy.

### 4.6 What replaces "Maintainer at 1000 merit"

In the open-source template, Maintainer becomes a position (rank 60) with `merit_eligibility = 1000`. Reaching 1000 merit files a nomination; a Maintainer or above approves it. This is a behavior change from today, flagged as decision D7.

---

## 5. Config schema and templates

### 5.1 Schema

The config is stored in tables (so it is audited and guarded), and can be exported and imported as one JSON document. The document shape:

```jsonc
{
  "org": { "slug": "eustress", "name": "Eustress", "mode": "open" },
  "policy_overrides": { "payouts.hold_days": 10 },
  "permissions_extra": [ { "key": "community.mentor", "class": "basic" } ],
  "merit_sources": [
    { "key": "github.pr_merged", "enabled": true, "weight": 1.0,
      "raw_points": { "score>=95": 60, "score>=90": 50, "score>=80": 35, "score>=70": 25, "score>=60": 15, "default": 10 },
      "daily_cap": 150, "vesting_days": 14, "external_counts": true },
    { "key": "github.review_approved", "enabled": true, "weight": 1.0, "raw_points": { "default": 10 },
      "daily_cap": 40, "vesting_days": 14, "per_pair_daily_cap": 2, "self_review": false }
  ],
  "tiers": [
    { "key": "newcomer", "label": "Newcomer", "min_merit": 0, "rank": 0,
      "permissions": ["profile.self", "community.read", "leaderboard.read"] },
    { "key": "contributor", "label": "Contributor", "min_merit": 50, "rank": 10,
      "min_account_age_days": 30, "requires_identities": ["github"],
      "permissions": ["community.post", "community.vote"], "discord_role_id": "..." }
  ],
  "positions": [
    { "key": "maintainer", "title": "Maintainer", "rank": 60, "default_term_days": 365,
      "merit_eligibility": 1000,
      "permissions": ["members.grant", "members.revoke", "payouts.read_all", "roster.read_full"] }
  ],
  "compartments": [ { "key": "cell-eng", "label": "Cell Engineering", "classification": "confidential" } ],
  "discord": { "guild_id": "...", "alert_channel_id": "...", "audit_anchor_channel_id": "...",
               "managed_role_ids": "derived from tiers and positions" }
}
```

Import is a `policy.change` guarded action with the same diff preview as a mode switch.

### 5.2 Templates

Templates are JSON files in `config/templates/`. Applying one to a new org is an import. They contain no code.

**Open-source project** (`mode: open`)

| Kind | Name | Rank | Gate |
|---|---|---|---|
| tier | Newcomer | 0 | merit 0 |
| tier | Contributor | 10 | merit 50, GitHub linked, account 30 days |
| tier | Active Contributor | 20 | merit 150 |
| tier | Core Contributor | 30 | merit 500 |
| position | Reviewer | 40 | appointed |
| position | Maintainer | 60 | appointed; nomination at merit 1000 |
| position | Lead Maintainer | 80 | appointed, two-person, 1-year term |
| config | Owner | 1000 | env only |

**Closed-source company** (`mode: closed`)

| Kind | Name | Rank | Gate |
|---|---|---|---|
| tier | Member | 0 | merit is shown, confers no rank (ceiling 0) |
| position | Contractor | 10 | appointed, 90-day term, compartment-scoped |
| position | Engineer | 20 | appointed |
| position | Senior Engineer | 30 | appointed, two-person |
| position | Head of Cell Engineering | 60 | appointed, two-person, compartment `cell-eng` |
| position | Director | 70 | appointed, two-person |
| position | Board Member | 90 | appointed, two-person, 1-year term |
| config | Owner | 1000 | env only |

**Hybrid (open core)** (`mode: hybrid`): open-source tiers for public repos (external merit counts from public repos only), closed-source positions for staff, compartments for internal work. Payouts for verified members.

**Merit DAO** (`mode: open`, overrides): `merit_rank_ceiling: 70`, `merit_may_confer_authority: true`, tiers up to "Steward" (rank 70) carry `members.grant` and `payouts.approve`. To compensate: vesting 30 days, `min_account_age_days: 90` for any authority tier, two-person on everything at rank 50 and above, `per_pair_daily_cap` on reviews, and clawback on revert. This is the only template where merit buys authority, and it says so in its own description.

---

## 6. Root account SSO

### 6.1 Tables

- `accounts`: `id` (`acc_` plus 24 random hex), `display_name`, `status` (`active`, `suspended`, `banned`), `created_at`, `suspended_at`, `suspended_reason`.
- `identities`: `id` (`idn_` plus random), `account_id`, `provider` (`discord`, `github`, `google`, `email`, `x`), `subject` (the provider's stable id: Discord snowflake, GitHub numeric user id, Google `sub`, normalized email, X user id), `email` (set only when verified), `handle` (display only, never used for lookup), `provider_created_at` (account age for sybil checks), `verified_at`, `created_at`, `last_login_at`. `UNIQUE (provider, subject)`. Partial unique indexes: one `github`, one `discord`, one `x` per account.
- `identity_events`: created, linked, auto_linked, unlinked, refused; shown to the member.
- `sessions`: `id` (SHA-256 of the cookie token), `account_id`, `identity_id`, `created_at`, `expires_at`, `revoked_at`, `last_used_at`, `ip`, `user_agent`.
- `email_tokens`: SHA-256 of a one-time token, normalized email, `link_account_id`, `expires_at` (15 min), `used_at`. Single use is enforced in the same UPDATE that checks it.
- `auth_rate`: fixed-window counters keyed by hashed email and by IP.

`contributors` gains a nullable `account_id`. It stays the per-GitHub merit and payout record during the transition; the account is the member.

### 6.2 Linking rules (ported from Weave `identity.rs`, as rules, not code)

A pure `decide(facts) -> Decision` function, tested branch by branch:

1. **Explicit link** from a signed-in session: attach to that account. Refuse if the identity belongs to another account (never merged). Refuse a second GitHub, Discord or X identity.
2. **Existing identity** signs in to its account.
3. **X never auto-links.** X gives no verified email.
4. **Auto-link only on a provider-verified email that matches exactly one account.** Zero or several matches create a new account. An unverified email never links anything.
5. **Discord and GitHub never auto-link** to an existing account either: they attach only by explicit link from a signed-in session. Rationale: these identities carry merit and payout attribution, so the cost of a wrong merge is money.
6. **Unlink is refused** if it is the account's last identity, and refused for the GitHub identity while the account has unpaid ledger rows.
7. **Rank and permissions are never read from an identity row.** A test asserts no query in `authz` joins `identities`.

### 6.3 Flows

- **In Discord**: the bot interaction proves the Discord identity. `/link` no longer takes a username. It replies with an ephemeral one-time URL (signed, single use, 10 minutes, bound to that Discord id). The URL starts GitHub OAuth; the GitHub numeric id from the OAuth response is what attaches.
- **On the Website or Desktop**: sign in with Discord, GitHub or Google OAuth, or an email link. DSync hosts the OAuth callbacks and issues the session (decision D1).
- **Legacy rows**: each existing contributor becomes an account with a `discord` identity (verified: the id came from a real interaction) and a `github` identity marked unverified (`verified_at NULL`, subject = login). Unverified GitHub identities keep their merit visible but cannot receive payouts and cannot satisfy `requires_identities`. Re-verifying through OAuth replaces the login subject with the numeric id.

---

## 7. Cross-cutting controls

### 7.1 Guarded actions (two-person rule)

One generic table, `guarded_actions`: `id`, `kind`, `payload_json`, `payload_hash`, `proposed_by`, `target_account_id`, `beneficiary_account_id`, `required_approvals`, `min_approver_rank`, `state` (`proposed`, `approved`, `executed`, `rejected`, `expired`, `failed`), `expires_at`. `guarded_action_approvals`: `UNIQUE (action_id, approver_id)`.

Execution checks, in one function: approver is not the proposer, not the target, not the beneficiary; approver rank meets `min_approver_rank`; approver holds the kind's approval permission at approval time; the payload hash is unchanged; the count is met; the action is unexpired. Execution claims the row with `UPDATE ... WHERE state = 'approved'` so it runs once.

Kinds and default thresholds:

| Kind | When guarded |
|---|---|
| `position.grant` | rank at or above `grants.two_person_min_rank` |
| `position.revoke` | same |
| `member.suspend`, `member.ban` | target rank at or above 30, or any ban |
| `payout.approve` | always one approver who is not the payee; two at or above `payouts.two_person_min_cents` |
| `payout.destination_change` | always (a changed Stripe account) |
| `merit.award_manual`, `merit.clawback` | above 50 points |
| `policy.change` | always |
| `consumer.create`, `consumer.scope_change` | always |
| `breakglass.activate` | never blocked, always alarms (7.5) |

Payout approval moves into this table. `ledger::approve` keeps its atomic claim and Stripe idempotency key unchanged; it is called by the guarded-action executor instead of directly by the route. The shared `DSYNC_ADMIN_TOKEN` stops being able to approve: it can list and propose only (decision D5). `decided_by` comes from the authenticated account, never from the body.

### 7.2 Append-only, hash-chained audit log

`audit_log`: `seq INTEGER PRIMARY KEY`, `at`, `actor_account_id`, `actor_kind` (`account`, `system`, `consumer`, `breakglass`), `action`, `target`, `payload_json`, `prev_hash`, `hash`.

- `hash = SHA-256(prev_hash || seq || at || actor || action || target || canonical_json(payload))`.
- SQLite triggers `BEFORE UPDATE` and `BEFORE DELETE` on `audit_log` raise an abort.
- Every authority change writes its audit row in the same transaction as the change. No audit row, no change.
- **Anchoring**: every hour, and on every guarded action, the bot posts `seq` and `hash` of the head to a private Discord channel and (optionally) emails it to owners. Someone with raw DB file access can drop the triggers and rewrite the whole chain consistently; the anchors outside the DB are what catch that. This limit is stated, not hidden.
- `GET /api/v1/audit/verify` (permission `audit.verify`) walks the chain and compares with the latest anchors.

### 7.3 Discord reconciliation (Discord is a projection, never truth)

- **Managed roles** are the Discord role ids configured on tiers and positions. Everything else in the guild is untouched.
- **Expected state** per member = the managed roles their DSync account should hold.
- **Triggers**: `GUILD_MEMBER_UPDATE` events (the `GUILD_MEMBERS` intent is already on), every merit or grant change, and a full sweep every 15 minutes.
- **On drift**:
  - Extra managed role (someone added it by hand): remove it, write `audit: discord.drift_reverted`, fetch the guild audit log entry (`MEMBER_ROLE_UPDATE`) to name the Discord executor, post to the alert channel, DM owners. The executor is flagged for review.
  - Missing managed role: re-add it, audit, alert at lower severity.
  - Managed role edited (`GUILD_ROLE_UPDATE`: permissions, position, name) or deleted: alert at high severity; do not fight a deletion, mark the tier or position as unprojected until fixed.
- **Anti-flap**: the same drift on the same member three times in an hour stops auto-revert for that member and escalates. DSync does not enter an edit war with a guild admin; it records one.
- **Authority roles are never projected from merit**: at config time DSync refuses to map a tier to a Discord role that has `ADMINISTRATOR`, `MANAGE_ROLES`, `MANAGE_GUILD`, `BAN_MEMBERS`, `KICK_MEMBERS` or `MANAGE_WEBHOOKS`, unless `merit_may_confer_authority` is true.
- **Stated limit**: a Discord member with Administrator can always change roles. DSync cannot prevent it. It guarantees that the change is reverted, recorded and reported, and that it grants nothing in DSync or in any consumer.

### 7.4 Compartments (need-to-know)

- `compartments`: `org_id`, `key`, `label`, `classification` (`internal`, `confidential`, `restricted`).
- `compartment_members`: `account_id`, `compartment_id`, `via` (`position` or `direct`), `granted_by`, `justification`, `expires_at`.
- Roster, profile and position reads in `need_to_know` mode return only members who share a compartment with the viewer, plus the viewer's chain of command.
- Members on probation cannot join `confidential` or `restricted` compartments.

### 7.5 Break-glass

- One sealed credential (an owner account flagged `breakglass`, or a separate env key), usable only from an explicit `/api/v1/breakglass/activate` call.
- Activation grants owner-level access for 60 minutes, cannot be extended (only re-activated), and immediately: writes audit, posts to the alert channel, DMs and emails every owner, anchors the audit head.
- Every action taken under break-glass is audited with `actor_kind = breakglass`.

### 7.6 Suspension kill switch

`member.suspend` (guarded above rank 30) does, in one transaction plus queued effects:

1. `accounts.status = 'suspended'`; every session `revoked_at = now`.
2. Every pending ledger row for that account moves to `held`.
3. Discord reconcile removes every managed role on the next event (within seconds).
4. Consumer lookups return `status: suspended` with an empty permission list; consumer caches are at most 60 seconds (section 8.5).

---

## 8. Voltec contract (first consumer)

### 8.1 Endpoints

```
GET /api/v1/members/by-discord/{discord_id}?scopes=community.*
GET /api/v1/members/{subject}?scopes=community.*
```

`{subject}` is the pairwise id DSync issued to that consumer (8.4). Never public. No list or search endpoint exists for consumers.

### 8.2 Authentication: HMAC with timestamp and nonce

Request headers:

```
X-DSync-Consumer:  voltec
X-DSync-Key-Id:    k1
X-DSync-Timestamp: 1794470400            (unix seconds)
X-DSync-Nonce:     <22+ chars base64url, 128 random bits>
X-DSync-Signature: v1=<hex HMAC-SHA256>
```

String to sign (newline separated, exact):

```
v1
GET
/api/v1/members/by-discord/123456789012345678?scopes=community.*
1794470400
<nonce>
<hex SHA-256 of the body; the empty-string hash for GET>
```

Server checks, in order, all failing to the same `401 {"error":"unauthorized"}`:

1. Consumer exists and is active; key id is one of its active keys (two may be active during rotation).
2. `|now - timestamp| <= 300`.
3. Constant-time comparison of the signature.
4. Nonce is new: `INSERT` into `consumer_nonces (consumer, nonce)` with `UNIQUE`; a duplicate is a replay. Nonces older than 10 minutes are swept.
5. Per-consumer rate limit (default 60 requests per minute).

Secrets come from env (`DSYNC_CONSUMER_SECRET_VOLTEC_K1`), are at least 32 random bytes, and are never stored in the DB.

### 8.3 Scopes

- `api_consumers`: `key`, `label`, `scope_allowlist` (for Voltec: `["community.*"]`), `field_allowlist` (for Voltec: `tier`, `positions.title`, `merit.vested`, `permissions`), `active`, `created_by`.
- Effective scopes = requested scopes ∩ allowlist. Requesting nothing outside the allowlist is the normal case; requesting outside it is not an error, it simply returns nothing more, and is logged.
- Permissions returned are filtered server-side to the effective scopes. A member who holds `payouts.approve` in DSync shows nothing of it to Voltec.

### 8.4 Response

```json
{
  "subject": "vt_3f9c1e...",
  "status": "active",
  "tier": { "key": "contributor", "label": "Contributor", "rank": 10 },
  "positions": [ { "title": "Maintainer", "rank": 60, "expires_at": "2027-11-01T00:00:00Z" } ],
  "merit": { "vested": 640 },
  "permissions": ["community.read", "community.post", "community.vote"],
  "scopes_granted": ["community.*"],
  "policy_version": 7,
  "issued_at": "2026-11-12T19:00:00Z",
  "cache_ttl_seconds": 60
}
```

- `subject` = `"vt_" + hex(HMAC-SHA256(consumer_subject_key, account_id))[:32]`. Voltec keys its users by this. Two consumers cannot correlate members by id, and Voltec never learns the DSync account id.
- Unknown member, member not visible to this consumer, and member without a Discord identity all return the same `404 {"error":"not_found"}`, so the endpoint is not an enumeration oracle.
- Suspended or banned: `200` with `status` set and `permissions: []`, so Voltec revokes instead of keeping a stale grant.
- Fields outside `field_allowlist` are omitted, not nulled.

### 8.5 Voltec's side of the contract

- **The standing rule**: DSync standing maps only to `community.*` permissions in Voltec. It NEVER reaches manufacturer applicant data, messages, user management, roles, or super admin. Voltec enforces this in its own code with a fixed allowlist (`community.*`), independent of anything DSync returns, and a Voltec test asserts that a DSync response listing `applicants.read`, `messages.read`, `users.assign_role` or `super` grants none of them.
- Voltec caches a response for at most `cache_ttl_seconds` and re-checks on each session refresh.
- Voltec verifies nothing about DSync's TLS beyond normal HTTPS, so the DSync base URL is configuration, never user input.

### 8.6 Spy controls on the consumer channel

Every consumer lookup is audited (consumer, subject, scopes). A consumer exceeding its lookup baseline (default: more than 3 times its 7-day hourly median, or more than 500 distinct subjects per hour) trips an alert and, in closed mode, suspends the consumer key pending review. A stolen consumer secret is a bulk-read tool; this is how it is noticed.

### 8.7 First-party consumers: the Website (Leptos on Cloudflare) and Desktop

The Website rebuild and the Desktop app are first-party clients, but they get no shortcut around this design:

- **They never hold authority of their own.** No role strings, no ladders, no admin lists in their databases. They render what DSync returns and send actions to DSync.
- **Human actions carry the human.** The Website signs a member in through DSync (decision D1), holds a DSync session, and calls DSync on the member's behalf with that session. DSync sees the real actor for every grant, ban, vote and payout decision; the shared `DSYNC_ADMIN_TOKEN` pattern in `payouts_admin.ex`, where the bot never learns who clicked approve, does not carry over.
- **Server-to-server reads** (for example SSR rendering a member page) use the same HMAC scheme as Voltec (section 8.2) with a `website` consumer whose allowlist covers what the Website displays, filtered by the viewer's own permissions.
- **Gates are permissions.** The Website's admin pages check `can(viewer, "payouts.approve")`, `can(viewer, "members.ban")` and so on from DSync's `effective()` output, never `current_role in [...]`.
- **Cloudflare specifics**: the DSync base URL and consumer secret are Worker secrets, never `vars`; the Worker verifies nothing from the browser that DSync would not verify again.
- **Desktop** uses the same session-based API as the Website (Desktop README.md:36: same APIs and permissions for Web and Desktop), with the OAuth callback through a loopback redirect or device code, decided when Desktop gets past scaffold.

---

## 9. Threat model

Status column: **exists** (in code today), **partial**, **missing**. Each test is named as it will appear in `cargo test`; the mutation is the deliberate change that must make that test fail.

### 9.1 Spies (exfiltration by a legitimate-looking member or infiltrator)

| ID | Attack | Control | Status | Test | Mutation that must fail it |
|---|---|---|---|---|---|
| S1 | Enumerate the whole roster, merit and roles from the public API | Roster and leaderboard gated by `visibility.*`; `/api/contributor/stats` requires a session and returns only what policy allows | missing (H5) | `authz::closed_mode_hides_roster_from_non_members` | Return `Visibility::Public` from `mode_defaults(Closed)` |
| S2 | Join, behave, then pull confidential data in the first week | Probation mask: no `sensitive` permissions, no confidential compartments | missing | `authz::probation_strips_sensitive_permissions` | Skip the probation mask in `effective()` |
| S3 | Read across teams beyond need-to-know | Compartments in `need_to_know` mode | missing | `authz::need_to_know_roster_limited_to_shared_compartments` | Drop the compartment join in the roster query |
| S4 | Bulk-read profiles or export at night | Per-account read counters, threshold from policy, alert and freeze reads | missing | `anomaly::bulk_reads_trip_and_freeze_in_closed_mode` | Change `>` to `>=` on a never-reached sentinel, or remove the freeze branch |
| S5 | Leak a confidential document and deny it | Per-viewer watermark id and canary tokens issued by DSync; canary hit endpoint maps the token to the viewer | missing; **designed, deferred** until DSync serves confidential material | `canary::token_maps_back_to_viewer` | Issue the same token to two viewers |
| S6 | Correlate members across consumer apps | Pairwise subject ids | missing | `consumers::subject_is_pairwise_and_stable` | Return the raw account id |
| S7 | Use a stolen consumer secret to enumerate members | No list endpoint, uniform 404, lookup anomaly detection | missing | `consumers::unknown_and_hidden_return_identical_404` | Return 403 for hidden members |
| S8 | Learn positions or titles of a closed org | `visibility.positions` | missing | `authz::positions_hidden_in_closed_mode` | Ignore the field in the profile serializer |

### 9.2 Traitors (a trusted insider, possibly an admin, who turns)

| ID | Attack | Control | Status | Test | Mutation that must fail it |
|---|---|---|---|---|---|
| T1 | Admin grants themself or an ally a higher position | Strict-inequality rank, no self-grant | missing | `grants::cannot_grant_at_or_above_own_rank` and `grants::cannot_grant_to_self` | Change `<` to `<=` in `can_grant`; delete the self check |
| T2 | Admin approves their own high-rank grant, payout or policy change | Two-person rule, approver not proposer, not target, not beneficiary | missing (H3) | `guarded::proposer_cannot_approve`, `guarded::payee_cannot_approve_own_payout` | Remove the `approver != proposer` check |
| T3 | Proposal edited after the second person reviewed it | `payload_hash` bound at approval | missing | `guarded::edited_payload_invalidates_approval` | Skip the hash comparison |
| T4 | Rogue Discord admin hand-assigns a Maintainer role | Reconciliation reverts, audits, alerts; Discord roles never read as truth | missing (H8) | `reconcile::extra_managed_role_is_removed_and_alerted`, `reconcile::discord_role_never_grants_dsync_permission` | Make reconcile add-only |
| T5 | Insider deletes or edits audit rows to hide a grant | Append-only triggers, hash chain, external anchors | missing | `audit::update_and_delete_are_rejected`, `audit::tampered_row_breaks_verify` | Drop the trigger; skip `prev_hash` in the hash input |
| T6 | Grant made "temporarily" and never removed | Mandatory expiry above threshold, sweeper | missing | `grants::high_rank_grant_requires_expiry`, `grants::expired_grant_confers_nothing` | Treat `expires_at` as optional; ignore expiry in `effective()` |
| T7 | Admin mints an owner | Owners from env only; no code path writes rank 1000 | missing | `grants::owner_rank_unreachable_through_api` | Allow `P.rank == 1000` in validation |
| T8 | Admin silently switches the org to open to expose the roster | Mode switch is a guarded `policy.change` with diff preview | missing | `policy::mode_switch_requires_second_approver` | Apply on proposal |
| T9 | Admin revokes a peer or superior to seize control | `can_revoke` requires target rank below actor | missing | `grants::cannot_revoke_peer_or_superior` | Change `<` to `<=` in `can_revoke` |
| T10 | Break-glass used quietly | Activation always alarms and expires | missing | `breakglass::activation_writes_audit_and_alert` | Skip the alert enqueue |
| T11 | `/sync` run by a non-admin through a DM or an error path | Fail closed: deny unless the permission is proven | partial, fail open (H4) | `discord::sync_denied_without_member_context` | Restore the current `if let` fall-through |
| T12 | Insider with the shared admin token approves payouts | Token can list and propose, never approve | missing (H2, H3) | `payments::admin_token_cannot_approve` | Route approval directly to `ledger::approve` |

### 9.3 Criminals (account takeover, payout fraud, merit farming, sybils, bribery)

| ID | Attack | Control | Status | Test | Mutation that must fail it |
|---|---|---|---|---|---|
| C1 | `/link github:<victim>` to steal merit, role and payouts | GitHub OAuth proof; `/link` takes no username; refuse identities owned by another account | **missing, live (H1)** | `identity::link_requires_oauth_proof`, `identity::cannot_attach_identity_owned_by_other_account` | Accept a username argument; make `decide` return `AttachTo` for an owned identity |
| C2 | Re-register a renamed GitHub login | Identity keyed by numeric GitHub id | missing (H6) | `identity::github_subject_is_numeric_id` | Look up by `handle` |
| C3 | Auto-link takeover through an unverified or ambiguous email | Weave rules: verified only, exactly one match, never X | missing | `identity::never_auto_links_unverified_email`, `identity::never_auto_links_x`, `identity::ambiguous_email_creates_new_account` | Drop the `verified_email` check; accept `[first, ..]` |
| C4 | Lock the owner out by unlinking their last identity | Unlink refused for the last one | missing | `identity::cannot_unlink_last_identity` | Change `< 2` to `< 1` |
| C5 | Replay a signed GitHub webhook | Delivery dedupe plus per-effect keys | **exists** | `duplicate_deliveries_do_not_double_award_merit` (exists) | Remove the delivery claim (verify the existing test fails) |
| C6 | Forge a webhook | HMAC verification before parsing | **exists** | `rejects_invalid_or_missing_signature` (exists) | Return `true` on missing header |
| C7 | Replay a consumer request | Timestamp window plus nonce uniqueness | missing | `consumers::replayed_nonce_rejected`, `consumers::stale_timestamp_rejected` | Skip the nonce insert; widen the window to infinity |
| C8 | Farm merit by opening and closing PRs | Opening earns nothing; merge only; daily caps | partial (H7) | `merit::opened_pr_awards_nothing`, `merit::daily_cap_enforced` | Restore the 5-point open award; skip the cap |
| C9 | Self-review or review rings | `self_review: false`; per-pair daily cap | missing | `merit::self_review_awards_nothing`, `merit::review_pair_cap` | Remove the author check |
| C10 | Bank merit, then revert the PR or get banned | Vesting and clawback | missing | `merit::pending_merit_not_counted`, `merit::clawback_reduces_balance_and_tier` | Count `pending` in the balance |
| C11 | Sybil accounts to farm or to vote | One GitHub and one Discord per account; tier gates on provider account age (GitHub `created_at`, Discord snowflake time) | missing | `sybil::young_github_account_blocked_from_tier`, `identity::second_github_identity_refused` | Ignore `min_account_age_days` |
| C12 | Redirect payouts by swapping the Stripe destination | Destination change is guarded and starts a hold | missing | `payments::destination_change_holds_pending_rows` | Skip the hold |
| C13 | Pay out before fraud is noticed | `hold_days` before approval | missing | `payments::cannot_approve_during_hold` | Skip the hold check |
| C14 | Double pay through a retry or a race | Atomic claim plus Stripe idempotency key | **exists** | `approve_pays_once_with_idempotency_key` (exists) | Remove `WHERE status = ?` from the claim |
| C15 | Duplicate ledger rows for one merge | `UNIQUE (pr_id, merge_sha)` | **exists** | `ledger_is_idempotent_on_pr_id_and_merge_sha` (exists) | Drop the unique constraint in a test-only schema |
| C16 | Stolen session keeps working after suspension | DB-checked sessions, suspension revokes all, consumer TTL 60 s | missing | `sessions::suspension_revokes_all_sessions` | Check only `expires_at` |
| C17 | Brute-force email links or flood endpoints | Rate limits per email hash and IP; Actix rate limiting on auth and consumer routes | missing | `auth::email_link_rate_limited` | Raise the limit to `u32::MAX` |
| C18 | Bribe one approver | Two approvers above thresholds, all approvals audited and anchored; bribery is mitigated, not prevented | missing | covered by T2, T3 | covered by T2, T3 |
| C19 | Cross-repo PR number collision credits the wrong PR | Key contributions by `(repo, pr_id)` | missing (H9) | `webhook::same_pr_number_in_two_repos_is_two_contributions` | Update by number only |

---

## 10. Migrations (all additive)

Each is a new file in `migrations/`. No existing table is dropped, rebuilt or has a column removed. `payout_ledger` and `processed_webhook_events` are not touched except where noted.

| Order | File | Creates | Touches existing |
|---|---|---|---|
| 1 | `2026111300_accounts_identities.sql` | `accounts`, `identities`, `identity_events`, `sessions`, `email_tokens`, `auth_rate` | `ALTER TABLE contributors ADD COLUMN account_id TEXT`; backfill one account per contributor, a verified `discord` identity where `discord_user_id` is set, an unverified `github` identity |
| 2 | `2026111301_audit_log.sql` | `audit_log`, `audit_anchors`, immutability triggers | none |
| 3 | `2026111302_orgs_policy.sql` | `orgs` (one row `default`, mode `open`), `policy_changes` | none |
| 4 | `2026111303_hierarchy.sql` | `permissions`, `tiers`, `tier_permissions`, `positions`, `position_permissions`, `position_grants`, `compartments`, `compartment_members` | seeds tiers from the current `merit_roles` rows (same thresholds, same Discord ids); `merit_roles` stays as a read-only legacy table |
| 5 | `2026111304_merit_ledger.sql` | `merit_sources`, `merit_events` | backfills `merit_events` from `merit_activities` as `vested` with `source = 'legacy'`; `ALTER TABLE contributions ADD COLUMN repo TEXT`, `ADD COLUMN pr_id INTEGER` |
| 6 | `2026111305_guarded_actions.sql` | `guarded_actions`, `guarded_action_approvals` | `ALTER TABLE payout_ledger ADD COLUMN payee_account_id TEXT`, `ADD COLUMN hold_until TEXT`; new status value `held` (status is free TEXT, no CHECK, so no rebuild) |
| 7 | `2026111306_consumers.sql` | `api_consumers`, `consumer_nonces`, `consumer_lookups` | none |
| 8 | `2026111307_discord_reconcile.sql` | `discord_drift_events` | none |

The one change worth stating plainly: adding the `held` status means code that lists `pending_approval` rows will not see held ones. That is the intent; the `/payouts` view will list both.

---

## 11. Build plan

Every phase ends with: `cargo build` clean, scoped `cargo test` for the module, then each new security test run once with its mutation applied (it must fail) and once reverted (it must pass), and the existing payout and dedupe tests green. The full suite runs once at the end of each phase. Output goes into the phase's commit message and the final report.

| Phase | Scope | Closes |
|---|---|---|
| **P0 hotfix** (no migration) | (1) `/link` refuses to bind a GitHub login that is already linked to a different Discord id, and refuses to change an existing binding. (2) `/payout-setup` stops creating new Stripe accounts until P1 proves GitHub ownership; contributors already onboarded keep their existing account, and the admin-token onboarding route still works for links an admin has confirmed. (3) `/sync` fails closed. (4) Stats return only the fields the open-mode default allows. (5) Website repo, separate change: payout approve on `/admin/ledger` limited to an explicit account allowlist instead of "Core Contributor or Maintainer". | H1 (contain), H2 (contain), H4, part of H5 |
| P1 | Accounts, identities, `decide`, `can_unlink`, GitHub OAuth, Discord one-time link, sessions | C1, C2, C3, C4, C16, H1 (fix) |
| P2 | Audit log, triggers, chain verify, Discord anchor | T5 |
| P3 | Orgs, policy, `mode_defaults`, templates, the `.mode` scan test | S1, S8, T8 (preview half) |
| P4 | Tiers, positions, permissions, `effective()`, `can_grant`, `can_revoke`, owners from env; replace hardcoded thresholds | T1, T6, T7, T9, H10 |
| P5 | Guarded actions; payouts through them; admin token demoted; holds | T2, T3, T8, T12, C12, C13, H2, H3 |
| P6 | Discord reconciliation, drift alerts, authority-role mapping refusal | T4, H8 |
| P7 | Merit ledger, sources, caps, vesting, clawback, sybil gates, repo-scoped contributions | C8, C9, C10, C11, C19, H7, H9 |
| P8 | Voltec endpoint, consumer registry, HMAC, nonces, pairwise ids, lookup anomaly | S6, S7, C7 |
| P9 | Compartments, probation, read anomaly detection, break-glass | S2, S3, S4, T10 |
| Deferred | Canary and watermark issuance (S5): needs confidential content served through DSync first | S5 |

Website and Desktop changes (switching their admin gate to DSync permissions, retiring the Website's own role ladders and `contributors` copy, its unauthenticated `/api` routes (H13) and its unauthenticated `bot_client.ex` calls) follow P1 (sessions) and P5 (guarded actions) and are tracked in the Website rebuild, not in this repo. The Website's voting rules (per-change votes, the banner must outrank the target) are ported as `required_approvals` and `min_approver_rank` on guarded actions rather than reimplemented in the Website.

---

## 12. Decisions for McKale

Each has a recommendation. None of them is implemented until you answer.

| # | Decision | Options | Recommendation |
|---|---|---|---|
| D0 | Ship the P0 hotfix now, before the rest of this design is approved? | (a) yes, today, no migration; (b) wait | **(a)**. H1 is exploitable by any guild member right now. |
| D1 | Where do accounts and sessions live? | (a) DSync-Discord owns accounts, identities, OAuth callbacks and sessions; Website and Desktop become clients; Supabase auth retired. (b) Supabase stays the identity provider and DSync trusts Supabase JWTs. | **(a)**. One authority. Supabase today only validates tokens by calling back, its config enables email signup by default, and it keeps a second role copy (H11). |
| D2 | Multi-org now? | (a) `org_id` on every new table, one `default` org row; (b) single-org schema | **(a)**. Costs one column; avoids a rebuild later. |
| D3 | Legacy `/link` bindings | (a) keep merit visible, block payouts until GitHub OAuth re-verification; (b) trust them | **(a)**. They were never proven. |
| D4 | Rank scale | (a) integers 0 to 1000, templates spaced by 10; (b) small integers | **(a)**, as Voltec does. |
| D5 | Shared `DSYNC_ADMIN_TOKEN` | (a) list and propose only, approvals need signed-in accounts; (b) keep as is | **(a)**. It is the single key to all payouts today. |
| D6 | Owners | (a) env only (`DSYNC_OWNER_ACCOUNT_IDS`); (b) a DB row | **(a)**. Defends T7. |
| D7 | Maintainer at 1000 merit | (a) becomes a position; 1000 merit files a nomination; (b) stays automatic | **(a)**. Today merit buys "full permissions" and, through the Website, payout approval. |
| D8 | Consumer subject ids | (a) pairwise per consumer; (b) raw account id | **(a)**. Voltec keys its rows by the pairwise id. |
| D9 | Discord drift response default | (a) auto-revert plus alert; (b) alert only | **(a)**, with the anti-flap stop. |
| D10 | Audit anchor destination | (a) private Discord channel; (b) plus email to owners; (c) external store | **(b)**. |
| D11 | Closed-mode external merit | (a) off; (b) on | **(a)**, per the brief. |
| D12 | Two-person thresholds | rank 60 open, 50 hybrid, 30 closed; payouts above $500 / $250 / always | Confirm or set numbers. |
| D13 | Payout hold | 7 days open, 14 closed | Confirm. |
| D14 | `github.pr_opened` merit | (a) retire; (b) keep but vest only on merge | **(a)**. |
