# DSync MCP server

DSync exposes its merit and payout functions to AI agents through a Model Context Protocol (MCP) server at `POST /mcp`. An agent connected to it can read the rubric, score merged work, read the leaderboard, and, if its token allows, prepare, approve and execute payout runs.

The server is the same engine as the REST API (`/api/v1`). It uses the same API tokens, the same scopes, and the same library functions. There is no MCP-only path to a payout or a score.

## 1. What the server is

- Transport: JSON-RPC 2.0 over HTTP. Every request is one `POST /mcp` with a JSON body. Every response is plain JSON (`Content-Type: application/json`).
- Stateless: no SSE stream, no sessions, no `Mcp-Session-Id`. `GET /mcp` returns 405.
- Protocol versions: `2025-06-18` (preferred), `2025-03-26`, `2024-11-05`. `initialize` echoes the version the client asks for when it is one of these, and offers `2025-06-18` otherwise. A request carrying an `MCP-Protocol-Version` header with any other value gets HTTP 400.
- Capabilities: `tools` only.
- Methods: `initialize`, `notifications/initialized` (HTTP 202, no body), `ping`, `tools/list`, `tools/call`. Any other method is JSON-RPC error -32601.
- Batches (a JSON array body) are refused with -32600.

### Error codes

| Situation | HTTP | JSON-RPC |
|---|---|---|
| No token, malformed token, wrong secret, revoked or expired token | 401, body `{"status":"error","message":"unauthorized"}` (the REST body) | none |
| Body is not JSON | 400 | -32700 parse error |
| Not a JSON-RPC 2.0 object, batch, bad `id`, missing `method`, `params` not an object | 400 | -32600 invalid request |
| Unknown method | 200 | -32601 method not found |
| Unknown tool, or arguments that do not match the tool's input schema (including any unknown field) | 200 | -32602 invalid params |
| The tool ran and failed (missing scope, execute disabled, rubric rejected the score, run not found, ...) | 200 | none: a tool result with `isError: true` |

Authentication is checked before anything else, so a request without a valid token gets 401 even when its body is malformed.

A tool failure message is always safe to show an agent. It never contains SQL, a file path, a token or a stack trace. Database and internal failures read `internal error`; the detail goes to the server log.

## 2. Creating a token for an agent

Tokens are created with `POST /api/v1/tokens`. The first token is created with the legacy `DSYNC_ADMIN_TOKEN` (bootstrap). After that, a member with `tokens.manage` can create tokens, but only with scopes they hold, and only for themselves or a member they strictly outrank.

```bash
curl -X POST https://dsync.example.org/api/v1/tokens \
  -H "Authorization: Bearer $DSYNC_ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
        "label": "scoring agent",
        "member_id": "agent-scorer",
        "scopes": ["merit.read", "merit.score"],
        "payee_keys": ["agent-scorer-gh", "gitlab:agent-scorer-gl"],
        "expires_at": "2027-01-01T00:00:00Z"
      }'
```

The response shows the token (`dsync_<id>_<secret>`) once. Store it in the agent's secret store. DSync keeps only its SHA-256.

- `member_id` is who the agent acts as. Every write the agent makes (a score, a run, an approval, an execution) is recorded as this member. No tool input can name a different actor, scorer or approver; an input that tries is rejected with -32602.
- `payee_keys` are the GitHub and GitLab logins this member is paid under and has authored work under. A key starting `gitlab:` is a GitLab login (`gitlab:<login>`, or `gitlab:<host>:<login>` for a self-managed host). Any other key is a GitHub login. DSync uses these keys to stop a payee approving their own payout and to stop a scorer scoring their own work. Declare every login the member uses.
- Give each agent the smallest set of scopes it needs, and an expiry.

## 3. Connecting a client

Point the client at `https://<your-dsync-host>/mcp` with the token as a bearer header.

Claude Code:

```bash
claude mcp add --transport http dsync https://dsync.example.org/mcp \
  --header "Authorization: Bearer dsync_0123456789abcdef_..."
```

A generic client configuration (JSON):

```json
{
  "mcpServers": {
    "dsync": {
      "type": "http",
      "url": "https://dsync.example.org/mcp",
      "headers": { "Authorization": "Bearer ${DSYNC_AGENT_TOKEN}" }
    }
  }
}
```

By hand with curl:

```bash
curl -s https://dsync.example.org/mcp \
  -H "Authorization: Bearer $DSYNC_AGENT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"curl","version":"1"}}}'

curl -s https://dsync.example.org/mcp \
  -H "Authorization: Bearer $DSYNC_AGENT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
```

Serve `/mcp` over HTTPS only. The bearer token is the whole credential.

## 4. Tools

`tools/list` returns only the tools the caller's token may use. `tools/call` checks the scope again, so a hidden tool is also refused. A refused call is a tool result with `isError: true` and the text `tool '<name>' requires scope '<scope>', which this token does not have`.

Every successful result carries the same JSON twice: as `structuredContent` and as a text block (`content[0].text`, pretty-printed).

| Tool | Scope | What it does |
|---|---|---|
| `rubric_get` | any valid token | The rubric `dsync-rubric-v1`: id, pinned hash, criteria, weights, level meanings, gates, score and points formulas, event rules |
| `merit_score_submit` | `merit.score` | Score one change against the rubric as the token's member |
| `merit_scores_list` | `merit.read` | Every score for one change, and the aggregate |
| `leaderboard_get` | `merit.read` | Totals, or one repository's XP, with the per-repository drilldown; private repository names included |
| `member_merit_get` | `merit.read` | One person's XP by repository and by event kind; private names included |
| `payouts_policy_get` | `payouts.read` | The payout policy in force |
| `payouts_ledger_list` | `payouts.read` | Payout ledger rows, optionally by status |
| `payouts_run_preview` | `payouts.prepare` | Dry run: the plan for a run; persists nothing |
| `payouts_run_create` | `payouts.prepare` | Create a prepared run |
| `payouts_run_approve` | `payouts.approve` | Approve every row of a run as the token's member, except the member's own rows |
| `payouts_run_execute` | `payouts.execute` | Pay the payable rows of a run through Stripe. Off unless the operator enables it (section 5) |
| `payees_list` | `payees.read` | Payees, optionally by status |
| `payee_get` | `payees.read` | One payee |
| `payorders_list` | `payorders.read` | Pay orders, optionally by status, with approvals and recent occurrences |
| `payorder_preview` | `payorders.read` | What the next occurrences of a pay order would pay; persists nothing |
| `payorder_create` | `payorders.manage` | Create a pay order as the token's member |
| `payorder_approve` | `payorders.approve` | Approve the current revision of a pay order as the token's member |
| `payorder_pause` | `payorders.manage` | Pause an active pay order |
| `invoices_list` | `invoices.read` | Invoices, optionally by status and payee, with approvals and history |
| `invoice_submit` | `invoices.submit` | Submit an invoice as the token's member |
| `invoice_approve` | `invoices.approve` | Approve a submitted invoice as the token's member |
| `payout_statement` | `payouts.read` | Everything paid or pending to one payee in a date range, by source |

`standing_get` and `hierarchy_get` (contract section 7) are not offered yet. They depend on CRA loading.

All tool inputs reject unknown fields.

### rubric_get

Input: `{}`.

```json
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"rubric_get","arguments":{}}}
```

Result (`structuredContent`, shortened):

```json
{
  "id": "dsync-rubric-v1",
  "hash": "b0e7b801aaf29a4c7b2cb4ac216673040af893bc538309dbc565f2a1bc4faa78",
  "criteria": [
    {"key": "impact", "weight": 30, "levels": [{"level": 0, "meaning": "no user or project effect"}, "..."]},
    "..."
  ],
  "min_evidence_chars": 20,
  "score_formula": "score = sum(weight * level / 4), 0..100, rounded to one decimal",
  "gates": {"ci_passed_false_caps_score_at": 40.0, "reverted_true_sets_score_to": 0},
  "points_formula": "points = base_points + round((max_points - base_points) * score / 100)",
  "rules": {"rubric": "dsync-rubric-v1", "scorers": ["agent"], "min_scorers": 1, "score_window_days": 7,
            "events": {"pr_merged": {"base_points": 10, "max_points": 60}, "...": "..."}}
}
```

The same document is public at `GET /api/v1/merit/rubric`.

### merit_score_submit

Input:

| Field | Type | Notes |
|---|---|---|
| `subject.provider` | `github` or `gitlab` | |
| `subject.host` | string, optional | Defaults to `github.com` or `gitlab.com` |
| `subject.repo` | string | `org/repo` or `group/sub/project` |
| `subject.subject_id` | string or integer | The global id of the pull or merge request (GitHub `pull_request.id`, GitLab `object_attributes.id`), not its number. `n<number>` when the id was unknown at ingest |
| `rubric_id` | string | Must be `dsync-rubric-v1` |
| `rubric_hash` | string | Must be the hash from `rubric_get` |
| `ci_passed` | boolean | `false` caps the score at 40 |
| `reverted` | boolean | `true` sets the score to 0 |
| `criteria` | object | Exactly `impact`, `correctness`, `difficulty`, `quality`, `risk`, `communication`, each `{"level": 0..4, "evidence": "at least 20 characters"}` |

The scorer is built from the token: id = the token's member, source = agent, GitHub and GitLab logins = the member's payee keys. The engine refuses a scorer who authored the change, a second score from the same scorer on the same change, an unknown change, and anything the rubric rejects. After storing the score it applies the once-only merit adjustment (contract section 6): when the first valid aggregate for a merged change arrives within `score_window_days` of the merge, the change's merit is re-priced once.

```json
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"merit_score_submit","arguments":{
  "subject": {"provider": "gitlab", "repo": "grp/proj", "subject_id": "555"},
  "rubric_id": "dsync-rubric-v1",
  "rubric_hash": "b0e7b801aaf29a4c7b2cb4ac216673040af893bc538309dbc565f2a1bc4faa78",
  "ci_passed": true,
  "reverted": false,
  "criteria": {
    "impact":        {"level": 3, "evidence": "fixes the duplicate payout reported in issue 41"},
    "correctness":   {"level": 1, "evidence": "no test; verified by reading the claim query"},
    "difficulty":    {"level": 1, "evidence": "a one-line change to an existing query"},
    "quality":       {"level": 0, "evidence": "adds a second copy of the claim logic"},
    "risk":          {"level": 0, "evidence": "touches the transfer path with no guard test"},
    "communication": {"level": 0, "evidence": "the merge request has no description at all"}
  }
}}}
```

Result:

```json
{
  "score": {
    "id": "6d1c...", "subject": {"provider": "gitlab", "host": "gitlab.com", "repo": "grp/proj", "subject_id": "555"},
    "scorer_id": "agent-scorer", "scorer_source": "agent",
    "rubric_id": "dsync-rubric-v1", "rubric_hash": "b0e7...", "score": 31.3,
    "ci_passed": true, "reverted": false, "detail": {"kind": "rubric", "criteria": ["..."]}, "created_at": "2026-11-12T18:00:00Z"
  },
  "aggregate": {"score": 31.3, "counted_scorers": 1, "min_scorers": 1},
  "merit_adjustment": {"score": 31.3, "old_points": 10, "new_points": 26, "delta": 16}
}
```

A refusal, for example a self-score:

```json
{"content":[{"type":"text","text":"a scorer may not score work they authored"}],
 "structuredContent":{"error":"a scorer may not score work they authored"},"isError":true}
```

REST: `POST /api/v1/merit/scores` with the same body (201, `{"status":"success","result":{...}}`).

### merit_scores_list

Input: a subject, `{"provider": "gitlab", "repo": "grp/proj", "subject_id": "555"}` (`host` optional).

Result: `{"subject": {...}, "scores": [ ...score records, oldest first... ], "aggregate": {"score": 31.3, "counted_scorers": 1, "min_scorers": 1}}`. `aggregate.score` is null when fewer than `min_scorers` counted scorers have scored.

REST: `GET /api/v1/merit/scores?provider=gitlab&repo=grp/proj&subject_id=555`.

### leaderboard_get

Input: `{}` for totals, or `{"provider": "github", "repo": "org/alpha", "limit": 10}` for one repository. `host` is optional. `limit` is 1 to 500, default 25.

Result:

```json
{
  "repo": null, "limit": 25, "include_private_names": true,
  "leaderboard": [
    {"rank": 1, "contributor_id": "4f0c...", "github_username": "alice", "current_role": "Newcomer", "contributions_count": 2,
     "total_points": 30,
     "by_repo": [
       {"kind": "repo", "label": "github.com/org/alpha", "provider": "github", "host": "github.com", "repo": "org/alpha", "private": false, "points": 15},
       {"kind": "repo", "label": "github.com/org/secret-sauce", "provider": "github", "host": "github.com", "repo": "org/secret-sauce", "private": true, "points": 15}
     ]}
  ]
}
```

The per-repository amounts always add up to `total_points`. Private repositories are named here (`private: true`). The public `GET /api/leaderboard` combines them into one `private repositories` line and never names them. Do not copy names from this view to a public place.

REST: `GET /api/v1/leaderboard?provider=&host=&repo=&limit=`.

### member_merit_get

Input: `{"contributor_id": "<uuid from a leaderboard row>"}`.

Result: `{"contributor_id": "...", "github_username": "alice", "total_points": 30, "by_repo": [...], "by_kind": [{"kind": "pull_request_merged", "points": 20}, {"kind": "pull_request_opened", "points": 10}]}`. Both lists add up to the total.

REST: `GET /api/v1/members/{contributor_id}/merit`.

### payouts_policy_get

Input: `{}`. Result: `{"source": "stored" | "default", "version": 3, "updated_by": "...", "updated_at": "...", "policy": {...}}`. The policy fields mirror `payouts.toml` (contract 3.5).

REST: `GET /api/v1/payouts/policy`.

### payouts_ledger_list

Input: `{}` or `{"status": "pending_approval"}` (`pending_approval`, `approved`, `paid`, `failed`, `rejected`). Result: `{"entries": [ ...ledger rows... ]}`.

REST: `GET /api/v1/payouts/ledger?status=`.

### payouts_run_preview

Input: `{}`. Result: `{"dry_run": true, "plan": {"enabled": true, "policy_version": null, "items": [{"ledger_id": "...", "payee_key": "alice", "amount_cents": 3000, "decision": "include", "reason": "approvals 0/1", "approvals": 0, "approvals_required": 1, "payable_now": false, "blockers": [...]}], "included_total_cents": 3000}}`. Nothing is stored and no money moves.

REST: `POST /api/v1/payouts/runs` with `{"dry_run": true}`.

### payouts_run_create

Input: `{}`. Result: `{"dry_run": false, "run": {"run": {"id": "...", "status": "prepared", "created_by": "<token member>", ...}, "items": [...]}, "plan": {...}}`. Refused while the policy is disabled. No money moves.

REST: `POST /api/v1/payouts/runs` with `{"dry_run": false}`.

### payouts_run_approve

Input: `{"run_id": "..."}`. Result:

```json
{"report": {"approver_member_id": "bob", "approved": ["<ledger id>"], "already_approved": [], "skipped_payee": [], "skipped": []}}
```

The approver is always the token's member. A row whose payee is this member (by any payee key on any of the member's tokens) is never approved and is listed in `skipped_payee`. No money moves.

REST: `POST /api/v1/payouts/runs/{id}/approve`.

### payouts_run_execute

Input: `{"run_id": "..."}`. Result:

```json
{"report": {"run_id": "...", "policy_version": null, "paid_cents": 3000, "run_status": "completed",
            "items": [{"ledger_id": "...", "outcome": "paid", "reason": null, "stripe_transfer_id": "tr_..."}]}}
```

Pays, through the existing transfer path, every row of the run that is payable now under the policy in force now. Limits are re-checked inside the claim that marks the row. A row that is not payable yet is reported as `not_payable` with the reason. A paid row is never paid twice (`already_paid`), and Stripe gets the idempotency key `dsync-ledger-<id>`.

REST: `POST /api/v1/payouts/runs/{id}/execute`.

### Payroll tools (contract section 11)

These mirror the payroll REST routes and call the same functions; `docs/PAYROLL.md` is the owner's guide. None of them moves money. Approving a pay order or an invoice only writes ledger rows; payout runs pay them, and `payouts_run_execute` keeps its guard.

- `payees_list` `{"status"?}` returns `{"payees": [...]}`. `payee_get` `{"payee_id"}` returns `{"payee": {...}}`. A payee has no bank or card field.
- `payorders_list` `{"status"?}` returns `{"payorders": [...]}`. Each order shows `approvals`, `approvals_counted`, `approvals_required`, `approved` and its recent `occurrences`.
- `payorder_preview` `{"order_id", "periods"?}` (1 to 24, default 3) returns `{"preview": {"items": [{"period_key", "timing", "split", "waiting_for"}]}}`. Persists nothing.
- `payorder_create` takes the body of `POST /api/v1/payorders`: `name`, `rule`, `schedule`, optional `currency`, `memo`, `draft`. Result `{"payorder": {...}}` with `created_by` set to the token's member. Refused when the caller is a payee of the order.
- `payorder_approve` `{"order_id"}` returns `{"result": {"result": "recorded" | "already_approved", "approver_member_id", "revision", "order"}}`. Refused (as a tool error) for the creator, the author of the latest change and any payee of the order.
- `payorder_pause` `{"order_id"}` returns `{"payorder": {...}}`. Resume and cancel are REST only.
- `invoices_list` `{"status"?, "payee_id"?}` returns `{"invoices": [...]}`.
- `invoice_submit` takes the body of `POST /api/v1/invoices`. The total must equal the lines plus tax. Result `{"invoice": {...}}` with `submitted_by` set to the token's member.
- `invoice_approve` `{"invoice_id"}` returns `{"result": {"result", "approver_member_id", "invoice"}}`. Refused for the payee and the submitter. With enough approvals the invoice is `scheduled` with its `ledger_id`.
- `payout_statement` `{"payee_id", "from", "to"}` (dates, inclusive) returns `{"statement": {"sources": [...], "paid_cents", "pending_cents"}}`.

Compensation is confidential. Give an agent the payroll read scopes only when it needs them, and never `payorders.manage` together with `payorders.approve`, or `invoices.submit` together with `invoices.approve`, on one token.

## 5. The execute guard

`payouts_run_execute` moves real money, so it is off by default on MCP.

- Set `DSYNC_MCP_ALLOW_EXECUTE=true` in the server environment and restart to enable it. Any other value, or no value, leaves it off. The flag is read once at startup.
- While it is off, the tool is not listed for anyone, even a token holding every scope, and a call returns `isError: true` with: `payouts_run_execute is disabled on this server. An operator must set DSYNC_MCP_ALLOW_EXECUTE=true and restart the server to enable it. ...`
- While it is on, the caller still needs `payouts.execute`.
- The REST route `POST /api/v1/payouts/runs/{id}/execute` is not affected by this flag.

## 6. What an agent must never be given

- **Never `payouts.approve` and `payouts.execute` on the same token, and never to the same member.** Approval is the human check on a payout. One agent holding both can approve a run and pay it with no second party. Keep execute with a person or a separate, tightly held token, and keep `DSYNC_MCP_ALLOW_EXECUTE` off unless that decision has been made.
- **Never `payouts.prepare` and `payouts.approve` and `payouts.execute` together.** That is the full payout path in one credential.
- **Never `payouts.policy`.** An agent that can rewrite the policy can lower `approvals_required`, remove limits, or shorten the hold. Policy changes belong to people.
- **Never `tokens.manage`.** It lets the holder mint more tokens with the scopes it holds.
- **Never a token without its payee keys.** A token for a member who is ever paid, or who authors work, must declare every GitHub or GitLab login that member uses. Without them DSync cannot tell that the member is scoring their own work (see section 7).
- **Never a token without an expiry** for an automated agent. Rotate it. Revoke it with `POST /api/v1/tokens/{id}/revoke` when the agent is retired.
- Give a scoring agent `merit.read` and `merit.score` only. Give a reporting agent `merit.read` and `payouts.read` only.

## 7. Known limit: self-score detection depends on payee keys

A scorer may not score work they authored (contract section 6). Today DSync decides who the scorer is on GitHub and GitLab from the payee keys declared on the member's tokens. A key starting `gitlab:` is a GitLab login; any other key is a GitHub login. DSync also follows linked GitHub and GitLab identities of the author's contributor record.

If a member's tokens declare no payee keys, DSync does not know the member's GitHub and GitLab logins, so it does not recognize the member as the author of their own work and accepts the score. This is a known limit. A later round supplies member identities from the CRA (`members.toml` `[member.identities]`) and closes it. The test `known_limit_scorer_without_payee_keys_is_not_recognized_as_author` pins today's behaviour so that change is made deliberately.

Until then, the mitigation is operational: always declare payee keys on tokens (section 6). Once merit rules load from the CRA (`merit.toml`), a `min_scorers` greater than 1 also means one scorer alone cannot set a score. Today the rules are the contract 3.4 defaults, with `min_scorers = 1`.

## 8. Operator checklist

1. Create one token per agent with the smallest scopes, its payee keys, and an expiry.
2. Leave `DSYNC_MCP_ALLOW_EXECUTE` unset unless an agent is meant to execute payouts.
3. Serve `/mcp` over HTTPS.
4. Watch the server log for `mcp auth refused` and for `internal error` details.
