# AGENTS.md: setting up MSync (MeritSync) for a human

You are an AI assistant. A person gave you this repository so you can set MSync up for them. Follow this file literally. When a step says to ask the human, ask and wait; never answer for them.

## What MSync is

1. MSync is one Rust binary, `msync`: a management hierarchy, merit and payroll engine.
2. The Central Rank Authority (CRA) is the folder `.msync/` in the organization's hierarchy repository: five TOML files, `org.toml`, `members.toml`, `roles.toml`, `merit.toml`, `payouts.toml`. See "Where the `.msync/` files live" below.
3. Ranks, titles, who may approve what, merit points and payout rules come from those files. A change to them is a reviewed pull request.
4. Discord and Slack are projections of the hierarchy (roles and user groups). Neither is the source of truth. Connect Discord, Slack or both; neither is allowed only in dev mode.
5. The Foundry API is the integration layer over GitHub and GitLab (webhooks, repository reads).
6. Surfaces: REST (`/api/v1`, described in `openapi.yaml`), MCP (`POST /mcp`), a signed portal sync API, and MSync as an OAuth/OpenID Connect provider.
7. Owners (rank 1000) are engine configuration (`MSYNC_OWNER_MEMBER_IDS`), never CRA content.
8. Every money action needs human approvals; the audit log is hash-chained.
9. Status: pre-release. There is no published binary; you build from source.
10. License: Apache 2.0.

## Questions for the human (never answer these yourself)

Ask all of these before you edit any template file. Record the answers; do not guess.

- Mode: the `org.toml` mode is `open`, `source_available`, `hybrid` or `closed`. The templates are `open-source`, `source-available`, `hybrid`, `closed-source` and `merit-dao` (an open-mode variant). Run `msync templates` to see the list; read `templates/<name>/README.md` for what each means.
- Roles, ranks and titles: what are the roles, what rank (0 to 999) does each carry, which chat role or group does each map to?
- Owners: who are the owners? Get each owner's login on GitHub or GitLab.
- Payouts: are there any? If yes: amounts, currency, limits, schedule and who approves.
- Chat platform: Discord, Slack, both, or none for now?
- Repositories: which repositories count for merit, which are mirrors or private?
- Where `.msync/` lives: in a private hierarchy repository (the recommended default), or in `.msync/` inside a shared work repository. Explain the difference below to the human and let them choose; never choose for them.

## Where the `.msync/` files live

**Where the `.msync/` files live.** There are two kinds of repository, and they are not the same thing.

- **The hierarchy repository** holds `.msync/`. It is a small, private repository that the owner manages, usually with an assistant such as Claude. Only the few people allowed to change the hierarchy get write access, so every change arrives as a reviewed pull request. Ordinary members and outside contributors never need access to it.
- **Work repositories** are where people write code. They are listed in `org.toml` under `[[foundry]]`. They can be public or private, on GitHub or GitLab, and they never contain `.msync/`. MSync only receives their events.
- **Compensation is in neither.** Pay orders, hourly rates, shifts, timesheets and invoices live in MSync's own database, behind scoped tokens.

Putting `.msync/` inside a shared work repository is allowed for a small open project, but it is not the recommended layout: everyone with write access to the code could then propose hierarchy changes, and the roster would be as visible as the code.

## Setup sequence

Run every command from the repository root. On Windows use Git Bash. Do not run a step until the one before it passed.

### Step 1: check the prerequisites

```bash
cargo --version && curl --version | head -1 && openssl version && python3 --version
```

Expected: four version lines. Rust comes from rustup (stable). bash, curl, openssl and python 3 (standard library only) are needed for the smoke test. If one is missing, tell the human; do not install system software without being asked.

### Step 2: build

```bash
cargo build
```

Expected: ends with `Finished`. No database and no `DATABASE_URL` is needed at build time. The binary is `target/debug/msync` (`target/debug/msync.exe` on Windows).

### Step 3: confirm the binary

```bash
target/debug/msync --help
```

Expected: the usage text listing `serve`, `init`, `check`, `plan`, `doctor`, `token create` and `templates`.

### Step 4: validate the bundled example

```bash
target/debug/msync check examples/cra/eustress-engine
```

Expected: an OK result and exit code 0.

### Step 5: run the smoke test

```bash
bash scripts/smoke-local.sh
```

It builds, starts the server in dev mode on a free port with a temporary database under `target/smoke-run/` and `examples/cra/eustress-engine` as the local CRA, runs `scripts/smoke.sh`, prints one `PASS` or `FAIL` line per check, and stops the server by the PID it started. A pass is every line `PASS`, no `FAIL`, exit code 0. If any line says `FAIL`, stop and report the line; do not edit a check to make it pass. Options: `MSYNC_SMOKE_BIN`, `MSYNC_SMOKE_RELEASE=1`, `MSYNC_SMOKE_TMP`, `MSYNC_SMOKE_KEEP=1`.

### Step 6: ask the questions above

Then continue.

### Step 7: write the hierarchy from a template

```bash
target/debug/msync init --template open-source --dir ./my-org
```

Use the template that matches the human's mode. Expected: it writes `./my-org/.msync/` (five TOML files, `README.md`, `gitlab-ci.yml`) and `./my-org/.github/workflows/msync-cra.yml`. It refuses to overwrite any existing file; if it refuses, choose another directory, never delete the human's files.

### Step 8: fill the placeholders

Edit the TOML files in `./my-org/.msync/` using the human's answers. Every `REPLACE_ME_...` value must be replaced. Do not put an owner in these files. Do not invent amounts, ranks or people.

### Step 9: validate

```bash
target/debug/msync check ./my-org
```

Expected: OK and exit code 0. On failure it prints every error with its file and reason; unfilled `REPLACE_ME_...` placeholders count as errors. Fix the values; never weaken a check or delete a rule to get a pass.

### Step 10: preview the change

```bash
target/debug/msync plan ./empty-dir ./my-org
```

An old directory with no `.msync/` means a first snapshot. Expected: a Markdown change preview and a "Who can approve" section. Show it to the human.

### Step 11: configure `.env` and run in dev mode (local only)

Create `.env` in the repository root (the only file that may hold secrets) with exactly these lines. Do not copy `.env.example`: its placeholder values would switch connectors on with fake credentials.

```
MSYNC_DEV_MODE=true
MSYNC_CRA_LOCAL_DIR=./my-org
MSYNC_OWNER_MEMBER_IDS=github:<the human's login>
SERVER_HOST=127.0.0.1
SERVER_PORT=8080
DATABASE_URL=sqlite://msync-dev.db
```

Then:

```bash
target/debug/msync serve
```

Expected: the server starts with a dev-mode warning. In a second shell:

```bash
curl -s http://127.0.0.1:8080/health
```

Expected: JSON containing `"dev_mode": true`.

### Step 12: create the first API token

Use the same `DATABASE_URL` the server uses.

```bash
target/debug/msync token create --label owner --member <member-id> --scopes hierarchy.read,merit.read,audit.read
```

For `<member-id>`, use the `id` of the member in `members.toml` whose `github` identity is the owner login in `MSYNC_OWNER_MEMBER_IDS`; that member then has owner rank. Expected: one token printed once. Give it to the human to store; do not write it into any tracked file. Then check it in the shell where the human pasted it:

```bash
export MSYNC_AGENT_TOKEN='<the token>'
```

```bash
curl -s -H "Authorization: Bearer $MSYNC_AGENT_TOKEN" http://127.0.0.1:8080/api/v1/hierarchy
```

Expected: JSON with `"status": "success"` and the hierarchy. A local CRA load acts like a direct push: the first snapshot is accepted for an owner; a later two-person change (policy, `merit.toml`, `payouts.toml`, roles at or above the two-person rank) is refused because it did not come through a reviewed pull request. To change those locally, restart with a fresh database. `POST /api/v1/cra/reload` (owner token, `hierarchy.read`) reloads the directory.

### Step 13: connect chat and repositories (only when the human asks)

Read `docs/DISCORD.md` and/or `docs/SLACK.md`, and section 14.2 of `docs/CRA_CONFIG.md`. How to connect each repository (webhook URL, events, secret, read token and its permissions, `MSYNC_CRA_REPO`, `[[foundry]]`) and today's limits: `docs/CRA_CONFIG.md` section 14.2. Put every value in `.env` only. Then:

```bash
target/debug/msync doctor
```

Expected: each connector listed as configured or not, with every missing variable named. It never prints a secret value. Outside dev mode, at least one chat platform is required (Discord: `DISCORD_TOKEN` and `DISCORD_APPLICATION_ID`; Slack: `MSYNC_SLACK_SIGNING_SECRET`). GitHub settings are optional; with no `GITHUB_WEBHOOK_SECRET` every GitHub webhook is refused.

### Step 14: a real deployment

Only when the human asks. Follow "For a real deployment" in `docs/QUICKSTART.md`. Do not push, deploy or open a pull request unless asked.

## Hard rules

- Never commit a secret. Never write a secret into any file other than `.env`. `.env.example` is the only env file tracked, and it holds names and placeholders only.
- Never set `MSYNC_MCP_ALLOW_EXECUTE` to `true`. Never give a token `payouts.execute` unless the human says so in chat.
- Never approve a pay order, invoice or payout on the owner's behalf, over REST, MCP or any other way.
- Never use dev mode (`MSYNC_DEV_MODE`, `MSYNC_CRA_LOCAL_DIR`) on a reachable host.
- Never push, deploy, create a repository or publish anything unless asked.
- Never put an owner in the CRA files. Owners come from `MSYNC_OWNER_MEMBER_IDS`.
- Never weaken a check, a test or a validation to make something pass. Report the failure.
- Never repeat a token or secret in chat after its creation step. Use Stripe test keys (`sk_test_...`) only; live Stripe needs the human's explicit word.
- Never kill processes by name. Stop the server by the PID you started.

## Where things live

| Need | File |
|---|---|
| Quickstart, clone to smoke test | `docs/QUICKSTART.md` |
| Prompts a human pastes to you | `docs/SETUP_WITH_AN_ASSISTANT.md` |
| Full platform contract | `docs/CRA_PLATFORM.md` |
| The five `.msync/` files | `docs/CRA_CONFIG.md` |
| Payroll, pay orders, invoices | `docs/PAYROLL.md` |
| Discord bot | `docs/DISCORD.md` |
| Slack app | `docs/SLACK.md`, `slack/manifest.yaml` |
| Portal sync API | `docs/PORTAL_API.md` |
| Sign-in and OAuth provider | `docs/SIGN_IN.md` |
| MCP server and "Connecting Claude" | `docs/MCP.md`, `mcp.example.json` |
| Threat model | `docs/HIERARCHY_AND_THREAT_MODEL.md` |
| REST API description | `openapi.yaml` |
| Templates | `templates/<name>/` |
| Worked example CRA | `examples/cra/` |
| Reporting a vulnerability | `SECURITY.md` |
| Smoke test | `scripts/smoke-local.sh`, `scripts/smoke.sh` |

## When you are done

Report to the human: which steps passed (with the output you saw), which decisions they made, which are still open, and what you did not do (for example no push, no deploy, no live keys).
