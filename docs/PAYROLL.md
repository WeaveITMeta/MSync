# Payroll in MSync (MeritSync): a guide for the business owner

MSync can pay the people and companies who work for you: salaries, stipends, bonuses, shared pools, commissions and contractor invoices. It also pays contributors for merged work (merit payouts) as before. Every payment, whatever its source, lands in one ledger and leaves through one path: a payout run that sends a Stripe transfer. The same approvals, limits, hold, blocks and kill switch apply to all of it.

This guide shows each task with its REST call (`/api/v1`, bearer token) and its MCP tool (`POST /mcp`). The contract behind it is section 11 of `docs/CRA_PLATFORM.md`.

## Before you start

- **Money is in cents.** $2,500.00 is `250000`.
- **Percentages are basis points.** 1% is `100`, 12.5% is `1250`, 100% is `10000`.
- **Dates are `YYYY-MM-DD`.** Times are UTC. A payment dated a day becomes payable at 00:00 UTC that day.
- **You act through a token.** Every write is recorded as the token's member. No request can name a different actor; a body with a field like `created_by` or `approver_member_id` is refused.
- **Compensation is confidential.** It is stored in MSync, behind scoped tokens, never in the CRA repository. The CRA still decides who may act: ranks, approver rules and payee identities come from it.

### Scopes

| Scope | Lets a token |
|---|---|
| `payees.read` | list and read payees |
| `payees.manage` | add and change payees |
| `payorders.read` | list, read and preview pay orders |
| `payorders.manage` | create, change, pause, resume and cancel pay orders |
| `payorders.approve` | approve pay orders |
| `invoices.submit` | submit invoices |
| `invoices.read` | list and read invoices |
| `invoices.approve` | approve, reject and void invoices |
| `payouts.read` | read the ledger and payee statements |

Paying still needs `payouts.prepare`, `payouts.approve` and `payouts.execute`, exactly as for merit payouts.

## 1. Add a payee

A payee is a person or a company you pay. It has a display name, an optional legal name, a kind (`individual` or `company`), an optional email, an optional CRA member id, its payee keys (the GitHub or GitLab logins it is paid under), its Stripe Connect account, its tax form status (`none`, `requested`, `on_file`) and notes.

MSync never stores a bank account or card number. There is no field for one, and a request that sends one is refused. The payee is paid into their Stripe Connect account; you pay from your own Stripe balance.

```
POST /api/v1/payees                    payees.manage
{"display_name": "Ana Ruiz", "kind": "individual", "email": "ana@example.com",
 "member_id": "ana", "payee_keys": ["anaruiz"], "tax_form_status": "on_file"}
```

- If a contributor with one of the payee keys has already finished Stripe onboarding, the payee takes that Stripe account and can be paid at once.
- You can give a Stripe account directly with `"stripe_account_id": "acct_..."`. It cannot receive money until Stripe confirms it (the `account.updated` webhook). Changing a payee's Stripe account switches payouts off again until Stripe confirms the new one. Treat `payees.manage` as a sensitive scope.
- Change a payee with `PATCH /api/v1/payees/{id}`. Set `"status": "paused"` to stop paying them for now, or `"closed"` to stop for good. A paused or closed payee, or one whose Stripe account cannot receive transfers, is never paid.
- Nobody can create or change the payee record that pays themselves.

Read payees: `GET /api/v1/payees?status=active`, `GET /api/v1/payees/{id}` (`payees.read`). MCP: `payees_list`, `payee_get`.

## 2. Pay a fixed monthly salary

A pay order is an instruction to pay: an amount rule and a schedule.

```
POST /api/v1/payorders                 payorders.manage
{"name": "Ana salary",
 "rule": {"kind": "fixed", "payee_id": "<ana>", "amount_cents": 500000},
 "schedule": {"kind": "repeat", "cadence": "monthly", "start": "2026-12-01", "count": 12}}
```

- Cadences: `weekly`, `biweekly`, `monthly`, `quarterly`. Add `"end": "YYYY-MM-DD"` (inclusive) or `"count"` or both; the first one reached ends the schedule.
- A monthly order started on the 31st pays on the last day of shorter months and goes back to the 31st when it can.
- The order pays nothing until it is approved (section 9). Dates already in the past are paid once it is approved, one payment per date.
- Add `"draft": true` to save it without it ever paying; resume it to start it.

MCP: `payorder_create` with the same arguments.

## 3. Pay a one-time bonus

```
POST /api/v1/payorders                 payorders.manage
{"name": "Launch bonus",
 "rule": {"kind": "fixed", "payee_id": "<ana>", "amount_cents": 100000},
 "schedule": {"kind": "once", "date": "2026-12-15"}}
```

It pays once, on that date, and then the order shows `ended`. MCP: `payorder_create`.

## 4. Split a pool by percentages

```
POST /api/v1/payorders                 payorders.manage
{"name": "Studio profit share",
 "rule": {"kind": "pool_percent", "pool_cents": 1000000,
          "shares": [{"payee_id": "<ana>", "bps": 5000}, {"payee_id": "<ben>", "bps": 3000}]},
 "schedule": {"kind": "repeat", "cadence": "quarterly", "start": "2027-01-01"}}
```

- Each payee gets the pool times their percentage, rounded down to the cent.
- The percentages may add up to less than 100%, never more. A request over 100% is refused.
- Whatever is not assigned, and every rounding cent, is not paid. It is reported as `leftover_cents` on the occurrence and never given to anyone. In this example 20% of the pool stays with you.

MCP: `payorder_create`.

## 5. Split a pool by XP

XP is the merit that people earn for their work in your repositories (through the Foundry API from GitHub and GitLab).

```
POST /api/v1/payorders                 payorders.manage
{"name": "Engine contributor pool",
 "rule": {"kind": "pool_by_xp", "pool_cents": 2000000,
          "repos": [{"provider": "github", "repo": "eustress/engine"}],
          "roles": [], "payee_ids": [], "min_xp": 20, "max_share_bps": 4000},
 "schedule": {"kind": "repeat", "cadence": "monthly", "start": "2027-01-01"}}
```

- For each date, MSync adds up the XP each person earned in the period since the previous date (the first period is one cadence step before the start). XP earned exactly on the payment date belongs to the next period.
- `repos` limits the XP to those repositories. `roles` limits it to people holding one of those merit roles or CRA roles. `payee_ids` limits it to those payees. Leave a list empty for no limit.
- `min_xp` is the XP needed to share at all. `max_share_bps` caps any one person's share; the cut part is not paid to anyone else.
- Each person gets the pool times their share of the XP, rounded down. Someone with XP but no payee record is counted, and their share is not paid. If nobody qualifies, nothing is paid and the occurrence records why.
- Leftover cents are reported, never paid.
- An XP pool with an empty `payee_ids` list could pay anyone, so a member who is a payee, or who has declared payee keys, cannot create, change or approve it.

MCP: `payorder_create`. Use `payorder_preview` to see what the next periods would pay; for a period still running it counts the XP earned so far.

A commission works the same way with `percent_of_amount`:

```
{"kind": "percent_of_amount", "payee_id": "<ben>", "bps": 1000, "base_cents": 4500000}
```

That pays 10% of $45,000. To give a different base for each date, use `"bases": {"2027-01-01": 3900000, "2027-02-01": 5100000}`. A date with no base waits (it is not skipped); add the base with a change to the order, which then needs approval again.

## 6. Process a contractor invoice

A payee submits their own invoice, or staff submit it for them.

```
POST /api/v1/invoices                  invoices.submit
{"payee_id": "<acme>", "number": "ACME-0042", "issue_date": "2026-11-20", "due_date": "2026-12-20",
 "lines": [{"description": "Level design, 40 hours", "quantity": 40, "unit_amount_cents": 7500}],
 "tax_cents": 0, "total_cents": 300000, "memo": "November", "attachment_ref": "drive:abc123"}
```

- The total must equal the lines (quantity times unit amount) plus tax, or the invoice is refused. Quantities are whole units.
- The number must be new for that payee (letter case does not matter).
- Approve: `POST /api/v1/invoices/{id}/approve` (`invoices.approve`). MCP: `invoice_approve`. The payee and the person who submitted it can never approve it.
- When it has the approvals the policy requires, it becomes `approved` and at once one ledger row, and shows `scheduled`. The row is payable from the due date. The next payout run pays it, and the invoice then shows `paid`.
- Reject a submitted invoice: `POST /api/v1/invoices/{id}/reject` with an optional `{"reason": "..."}`. Void one that is not paid yet: `POST /api/v1/invoices/{id}/void`; its ledger row is rejected with it. A rejected or void invoice is never paid. A paid invoice can never be paid again.
- Every change is recorded with who did it and when, in `events` on `GET /api/v1/invoices/{id}`.

Read: `GET /api/v1/invoices?status=&payee_id=`, `GET /api/v1/invoices/{id}` (`invoices.read`). MCP: `invoices_list`, `invoice_submit`, `invoice_approve`.

## 7. Pause, resume and cancel

```
POST /api/v1/payorders/{id}/pause      payorders.manage
POST /api/v1/payorders/{id}/resume     payorders.manage
POST /api/v1/payorders/{id}/cancel     payorders.manage
```

- **Pause** stops an active order. Dates that pass while it is paused are skipped for good; resuming does not pay them later. They are recorded as skipped.
- **Resume** continues a paused order from the moment you resume, or starts a draft.
- **Cancel** ends the order for good. Payments it already created stay in the ledger; reject them there if they must not be paid.
- To change an order, `PATCH /api/v1/payorders/{id}` with any of `name`, `memo`, `currency`, `rule`, `schedule`. A change to the rule, the payees, the schedule or the currency needs fresh approval. A new name or memo does not.

MCP: `payorder_pause`. Resume and cancel are REST only.

## 8. Read a statement

```
GET /api/v1/payouts/statement?payee=<id>&from=2026-01-01&to=2026-12-31      payouts.read
```

It lists everything recorded for that payee between the two dates (inclusive): pay order payments, invoices, and merit payouts under any of their payee keys. It groups them by source, with `paid_cents` and `pending_cents` for each source and in total. Pending means not paid yet (waiting for approval, being paid, or a failed transfer that will be retried). Rejected rows are listed but not counted. MCP: `payout_statement`.

## 9. Who must approve what

| Action | Who may do it | Who never may |
|---|---|---|
| Create or change a pay order | a member with `payorders.manage` | anyone the order pays |
| Approve a pay order | members with `payorders.approve` at or above the policy's `min_approver_rank` | its creator, the author of its latest change, anyone it pays |
| Pause, resume or cancel a pay order | a member with `payorders.manage` | anyone the order pays |
| Submit an invoice | the payee, or staff with `invoices.submit` | |
| Approve, reject or void an invoice | members with `invoices.approve` at or above `min_approver_rank` | the payee; the submitter cannot approve |
| Add or change a payee | a member with `payees.manage` | the payee themselves |
| Pay | payout runs, as for merit payouts | |

- An order or invoice needs the policy's `approvals_required` approvals, each from a different member at or above `min_approver_rank`. An approval below that rank is recorded but does not count. The same member approving twice counts once.
- With `approvals_required` of 1 the creator still cannot approve their own order, so every order needs two people. Set it to 2 or more for larger teams.
- When an order or invoice is approved, its approvals carry over to the ledger rows it creates. At payment time each one is checked again (rank, and that the approver is not the payee), exactly as for merit rows.
- Nothing here moves money by itself. Pay orders and invoices only write ledger rows. Payout runs pay them: in `prepare` mode someone executes the run, in `auto` mode the scheduler pays what is already payable. The MCP execute tool stays off unless an operator turns it on.

## 10. The kill switch and the other guards

These apply to every row, whatever its source:

- **Kill switch.** Set `"enabled": false` in the payout policy (`PUT /api/v1/payouts/policy`). Nothing is prepared or paid until you set it back. Pay orders keep recording their payments, and those are paid once you turn payouts back on.
- **Hold.** `limits.hold_days` delays every row that many days after it was recorded.
- **Limits.** `min_payout_cents`, `max_per_payout_cents`, `max_per_member_per_period_cents` and `max_per_run_cents` apply to salaries and invoices too. A salary above `max_per_payout_cents` is never paid; raise the limit or split the payment.
- **Blocks.** `POST /api/v1/payouts/blocks` with a payee key stops every payment to that person, including their pay orders and invoices under any of their keys.
- **Not before its date.** An occurrence is not payable before its date, an invoice not before its due date.
- **Payee status.** A paused or closed payee, or one whose Stripe account cannot receive transfers, is not paid.
- **Once only.** Each date of an order is recorded once, so a restart or a double tick of the scheduler can never pay twice. Each ledger row is paid once, with Stripe idempotency key `dsync-ledger-<id>`.

## Reference

```
GET/POST   /api/v1/payees                 GET/PATCH /api/v1/payees/{id}
GET/POST   /api/v1/payorders              GET/PATCH /api/v1/payorders/{id}
POST       /api/v1/payorders/{id}/approve | pause | resume | cancel
GET        /api/v1/payorders/{id}/preview?periods=3
GET/POST   /api/v1/invoices               GET /api/v1/invoices/{id}
POST       /api/v1/invoices/{id}/approve | reject | void
GET        /api/v1/payouts/statement?payee=&from=&to=
```

MCP tools: `payees_list`, `payee_get`, `payorders_list`, `payorder_preview`, `payorder_create`, `payorder_approve`, `payorder_pause`, `invoices_list`, `invoice_submit`, `invoice_approve`, `payout_statement`. See `docs/MCP.md`.
