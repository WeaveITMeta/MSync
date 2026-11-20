# Setting up MSync with an AI assistant

MSync (MeritSync) is built to be set up by an AI assistant such as Claude Code. The repository carries instructions for the assistant (`AGENTS.md` and `CLAUDE.md` at the root). You supply the decisions and the accounts; the assistant does the typing.

The assistant never decides for you: your mode, your ranks, your owners and your payout amounts are yours. It is told never to commit a secret, never to enable MCP payout execution, and never to approve a payment on your behalf.

## Before you start

Make these decisions. The assistant will ask for each one, so having them ready saves time.

- **Mode.** Is your work open (anyone may contribute), source available, hybrid, or closed? The templates are `open-source`, `source-available`, `hybrid`, `closed-source` and `merit-dao`; each has a one-page `README.md` in `templates/`.
- **Roles.** The titles you use, the rank (0 to 999) each carries, and which chat role or group each maps to.
- **Owners.** Who holds ultimate authority (rank 1000). Owners are named in the server's configuration with their GitHub or GitLab login, never in your repository files.
- **Payouts.** Whether you pay anyone, and if so amounts, currency, limits, schedule and who approves.
- **Chat platform.** Discord, Slack, both, or none for now.
- **Repositories.** Which repositories earn merit, and which one holds the hierarchy.
- **Where `.msync/` lives.** A private hierarchy repository (the recommended default), or `.msync/` inside a shared work repository. The difference is below.

## Where the `.msync/` files live

**Where the `.msync/` files live.** There are two kinds of repository, and they are not the same thing.

- **The hierarchy repository** holds `.msync/`. It is a small, private repository that the owner manages, usually with an assistant such as Claude. Only the few people allowed to change the hierarchy get write access, so every change arrives as a reviewed pull request. Ordinary members and outside contributors never need access to it.
- **Work repositories** are where people write code. They are listed in `org.toml` under `[[foundry]]`. They can be public or private, on GitHub or GitLab, and they never contain `.msync/`. MSync only receives their events.
- **Compensation is in neither.** Pay orders, hourly rates, shifts, timesheets and invoices live in MSync's own database, behind scoped tokens.

Putting `.msync/` inside a shared work repository is allowed for a small open project, but it is not the recommended layout: everyone with write access to the code could then propose hierarchy changes, and the roster would be as visible as the code.

Accounts you may need:

- A small private GitHub or GitLab repository to hold the hierarchy (the CRA). The assistant connects it and your work repositories as `docs/CRA_CONFIG.md` section 14.2 describes.
- A Discord application and bot, or a Slack app, if you want chat roles. Not needed to try MSync locally.
- A Stripe test-mode account, only if you want to try payouts. Use test keys (`sk_test_...`). Live money is a deliberate step you take yourself.

You also need Rust (stable, through rustup), Git, and for the smoke test bash (Git Bash on Windows), curl, openssl and python 3.

Open the cloned repository in your assistant, then paste one of the prompts below.

## Prompt A: set MSync up locally and show me it working

```text
Read AGENTS.md and follow it literally. Set MSync up locally and show me it working. Build it, run the smoke test and show me every PASS line, then start it in dev mode with the open-source template in ./my-org so I can see /health and /api/v1/hierarchy answer. Ask me for any decision you need; do not guess. Do not commit, push or deploy anything, and do not put any secret in a tracked file.
```

## Prompt B: fit the hierarchy to my organization

```text
Read AGENTS.md and docs/CRA_CONFIG.md. Fit the MSync hierarchy to my organization. Show me the five templates with msync templates, recommend one and say why, then ask me the decisions: mode, roles with ranks and titles, who the owners are, payout amounts, limits and approvers, which chat platform, and which repositories count. Never answer those for me. Run msync init, fill every REPLACE_ME placeholder with my answers, run msync check until it passes without weakening any rule, then run msync plan and show me the preview and who can approve it. Do not put owners in the files; tell me what to put in MSYNC_OWNER_MEMBER_IDS.
```

## Prompt C: connect Discord or Slack and my repositories

```text
Read AGENTS.md, then docs/DISCORD.md or docs/SLACK.md (I use: <Discord, Slack or both>) and the webhook section of docs/CRA_CONFIG.md. Connect my chat platform and my repositories to MSync. Put every environment variable in .env only, never in a tracked file, and never repeat a secret back to me. Tell me exactly which values I must copy from the Discord developer portal or the Slack app page, and wait for me to put them in .env. Then set up the GitHub or GitLab webhooks as the docs say, run msync doctor, and show me what is configured and what is still missing. Do not enable MSYNC_MCP_ALLOW_EXECUTE, do not use dev mode on a reachable host, and do not push or deploy unless I ask.
```

## Connecting the assistant to a running MSync

Once MSync runs, an assistant can read and score through the MCP server. See "Connecting Claude" in `docs/MCP.md` and the example `mcp.example.json`. Give the assistant a narrow token: read and score scopes only, never `payouts.execute`.

## Where to go next

- `docs/QUICKSTART.md`: the same path without an assistant, and what to change for a real deployment.
- `docs/CRA_CONFIG.md`: every field of the five hierarchy files.
- `docs/PAYROLL.md`: pay orders, invoices and payout runs.
- `SECURITY.md`: reporting a vulnerability.
