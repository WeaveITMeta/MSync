# MSync (MeritSync)

[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

MSync is a management hierarchy, merit and payroll engine. The Central Rank Authority (CRA) is the `.msync/` folder in your organization's hierarchy repository: it says who holds which rank and title, who may approve what, how merit is earned and who approves pay. MSync reads that folder and enforces it.

## Where the `.msync/` files live

**Where the `.msync/` files live.** There are two kinds of repository, and they are not the same thing.

- **The hierarchy repository** holds `.msync/`. It is a small, private repository that the owner manages, usually with an assistant such as Claude. Only the few people allowed to change the hierarchy get write access, so every change arrives as a reviewed pull request. Ordinary members and outside contributors never need access to it.
- **Work repositories** are where people write code. They are listed in `org.toml` under `[[foundry]]`. They can be public or private, on GitHub or GitLab, and they never contain `.msync/`. MSync only receives their events.
- **Compensation is in neither.** Pay orders, hourly rates, shifts, timesheets and invoices live in MSync's own database, behind scoped tokens.

Putting `.msync/` inside a shared work repository is allowed for a small open project, but it is not the recommended layout: everyone with write access to the code could then propose hierarchy changes, and the roster would be as visible as the code.

How to connect each repository (webhook URL, events, secret, read token and its permissions, `MSYNC_CRA_REPO`, `[[foundry]]`) and today's limits: `docs/CRA_CONFIG.md` section 14.2.

Discord and Slack are projections of the hierarchy, not the source of truth. MSync keeps their roles and user groups in line with the repository, and reverts and reports hand edits. Use Discord, Slack or both; running with neither is allowed only in dev mode on your own machine. The Foundry API is the integration layer over GitHub and GitLab: signed webhooks in, repository reads out.

## What it does

- **Hierarchy from a repository.** Five TOML files define the organization. A change to them is a reviewed pull or merge request, and the rules that need two people say so. `msync plan` previews any change and names who can approve it.
- **Merit.** Merged work is scored against a rubric and earns points per the rules in `merit.toml`. Merit can open tiers of rank; powers come only from positions someone appointed.
- **Payroll.** Pay orders, invoices, hourly wages with shifts and clock-in, payees and payout runs, all in one ledger, with approvals, limits, a hold and a kill switch. Stripe test mode first; live money is a deliberate setting.
- **Surfaces.** REST (`/api/v1`, see `openapi.yaml`), an MCP server (`POST /mcp`), a signed portal sync API for your other sites, and MSync as an OAuth/OpenID Connect provider.
- **Audit.** An append-only, hash-chained audit log that can be verified.
- **Built for assistants.** The repository is a template and a kit. Give it to an AI assistant such as Claude and it sets MSync up for you from `AGENTS.md`.

## Status

Pre-release. There is no published binary; build from source. APIs and file formats can still change.

## Quickstart

From clone to a passing smoke test in dev mode: [docs/QUICKSTART.md](docs/QUICKSTART.md). With an assistant: [docs/SETUP_WITH_AN_ASSISTANT.md](docs/SETUP_WITH_AN_ASSISTANT.md).

```bash
cargo build
```

```bash
bash scripts/smoke-local.sh
```

No database is needed to build. On Windows use Git Bash.

## Commands

One binary, `msync`:

| Command | What it does |
|---|---|
| `msync` or `msync serve` | Start the service |
| `msync init --template <name> --dir <path>` | Write `.msync/` and CI files from a template; refuses to overwrite |
| `msync check <dir>` | Validate `<dir>/.msync/`; every error with file and reason; exit 1 on failure |
| `msync plan <old-dir> <new-dir>` | Markdown change preview and who can approve it |
| `msync doctor` | Which connectors are configured, with every missing variable named; never prints a secret |
| `msync token create --label <text> --member <id> --scopes a,b` | Create an API token (printed once) |
| `msync templates` | List the templates |
| `msync --help`, `msync --version` | Usage and version |

Templates: `open-source`, `source-available`, `hybrid`, `closed-source`, `merit-dao`.

## Documentation

| Document | What it covers |
|---|---|
| [AGENTS.md](AGENTS.md), [CLAUDE.md](CLAUDE.md) | Instructions an AI assistant follows to set MSync up |
| [docs/QUICKSTART.md](docs/QUICKSTART.md) | Clone to smoke test, and what to change for a real deployment |
| [docs/SETUP_WITH_AN_ASSISTANT.md](docs/SETUP_WITH_AN_ASSISTANT.md) | What to prepare, and three prompts to paste |
| [docs/CRA_PLATFORM.md](docs/CRA_PLATFORM.md) | The full platform contract |
| [docs/CRA_CONFIG.md](docs/CRA_CONFIG.md) | The five `.msync/` files, field by field |
| [docs/PAYROLL.md](docs/PAYROLL.md) | Pay orders, invoices, hourly pay, payees and payout runs |
| [docs/DISCORD.md](docs/DISCORD.md) | Setting up the Discord bot |
| [docs/SLACK.md](docs/SLACK.md) | Setting up the Slack app |
| [docs/PORTAL_API.md](docs/PORTAL_API.md) | The signed portal sync API |
| [docs/SIGN_IN.md](docs/SIGN_IN.md) | Sign-in and MSync as an OAuth/OpenID Connect provider |
| [docs/MCP.md](docs/MCP.md) | The MCP server, its tools, and connecting Claude |
| [docs/HIERARCHY_AND_THREAT_MODEL.md](docs/HIERARCHY_AND_THREAT_MODEL.md) | Design and threat model |
| [docs/DiscordBot.md](docs/DiscordBot.md) | The hosted bot invite link |
| [docs/Configuration/GeneralInformation.md](docs/Configuration/GeneralInformation.md) | General configuration notes |
| [openapi.yaml](openapi.yaml) | Every REST route |
| [mcp.example.json](mcp.example.json) | Example `.mcp.json` for an MCP client |
| [SECURITY.md](SECURITY.md) | Reporting a vulnerability |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Contributing |
| [templates/](templates/) | Five starter hierarchies, each with a one-page README |
| [examples/cra/](examples/cra/) | Worked example organizations |
| [.env.example](.env.example) | Every environment variable, with placeholders only |

## Security

Every webhook is verified (GitHub and GitLab secrets, Slack signing secret, Stripe signature), every API call needs a scoped token, and payout approvals follow the rules in your CRA. No secret belongs in the repository; `.env.example` is the only env file tracked. See [SECURITY.md](SECURITY.md).

## License

Apache License 2.0. See [LICENSE](LICENSE).
