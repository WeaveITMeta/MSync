# Quickstart: from clone to a passing smoke test

This takes you from a fresh clone to a running MSync in dev mode. You need Rust (stable, through rustup), Git, curl, openssl and python 3 (standard library only). On Windows, run every command in Git Bash.

No database is needed at build time. A fresh clone builds with no `DATABASE_URL`. The database file is created on first start and migrations run automatically.

Dev mode is for your own machine only. Never use it on a host anyone else can reach.

## 1. Clone

```bash
git clone <your MSync repository URL> msync && cd msync
```

## 2. Build

```bash
cargo build
```

The binary is `target/debug/msync` (`target/debug/msync.exe` on Windows). Use `cargo build --release` for `target/release/msync`.

## 3. See the commands

```bash
target/debug/msync --help
```

The commands: `serve` (the default), `init`, `check`, `plan`, `doctor`, `token create`, `templates`.

## 4. Validate the bundled example

```bash
target/debug/msync check examples/cra/eustress-engine
```

Expected: OK and exit code 0.

## 5. Run the smoke test

```bash
bash scripts/smoke-local.sh
```

It builds, starts a server in dev mode on a free port with a temporary database under `target/smoke-run/`, loads `examples/cra/eustress-engine` as the local hierarchy, drives the real HTTP surface (signed webhooks, scores over REST and MCP, payout controls, a Slack command, a portal lookup, the audit chain, OpenID discovery), prints one `PASS` or `FAIL` line per check, and stops the server by its own PID. A pass is every line `PASS` and exit code 0. Options: `MSYNC_SMOKE_BIN`, `MSYNC_SMOKE_RELEASE=1`, `MSYNC_SMOKE_TMP`, `MSYNC_SMOKE_KEEP=1`. To test an already running server, see the header of `scripts/smoke.sh`.

## 6. Make your own hierarchy from a template

```bash
target/debug/msync init --template open-source --dir ./my-org
```

Other templates: `source-available`, `hybrid`, `closed-source`, `merit-dao` (`msync templates` lists them). This writes `./my-org/.msync/` plus `.msync/README.md`, `.msync/gitlab-ci.yml` and `.github/workflows/msync-cra.yml`. It refuses to overwrite any existing file.

Open the five TOML files in `./my-org/.msync/` and replace every `REPLACE_ME_...` value. Each file's comments say what to decide. Owners do not go in these files.

```bash
target/debug/msync check ./my-org
```

Expected: OK and exit code 0. Until every placeholder is replaced it fails and lists each one with its file and reason.

## 7. Configure a dev-mode `.env`

Create a file named `.env` in the repository root with exactly these lines. Do not start from a copy of `.env.example`: its placeholder values (for example `DISCORD_TOKEN=your_discord_bot_token_here`) would switch connectors on with fake credentials. `.env` is the only file that may hold secrets, and `msync` reads it from the directory you run it in.

```text
MSYNC_DEV_MODE=true
MSYNC_CRA_LOCAL_DIR=./my-org
MSYNC_OWNER_MEMBER_IDS=github:<your login>
SERVER_HOST=127.0.0.1
SERVER_PORT=8080
DATABASE_URL=sqlite://msync-dev.db
```

In dev mode a missing Discord or Slack setup is a warning, not an error. `MSYNC_CRA_LOCAL_DIR` loads the hierarchy from that directory as your owner entry. A local load acts as a direct push: the first snapshot is accepted, but a later two-person change (policy, `merit.toml`, `payouts.toml`, roles at or above the two-person rank) is refused. To change those locally, delete the database file and restart. `POST /api/v1/cra/reload` (owner token with `hierarchy.read`) reloads the directory.

## 8. Start the server

```bash
target/debug/msync serve
```

Expected: a startup warning that dev mode is on. Leave it running and open a second shell.

```bash
curl -s http://127.0.0.1:8080/health
```

Expected: JSON containing `"dev_mode": true`.

## 9. Create a token and read the hierarchy

Use the same `DATABASE_URL` as the server. For `--member`, use your member id from `./my-org/.msync/members.toml`.

```bash
target/debug/msync token create --label owner --member <member-id> --scopes hierarchy.read,merit.read,audit.read
```

The token prints once. Store it; MSync keeps only a hash.

```bash
export MSYNC_AGENT_TOKEN='<the token>'
```

```bash
curl -s -H "Authorization: Bearer $MSYNC_AGENT_TOKEN" http://127.0.0.1:8080/api/v1/hierarchy
```

Expected: JSON with `"status": "success"` and your hierarchy.

## 10. Preview a change

After you edit the hierarchy, `plan` shows what would change and who can approve it:

```bash
target/debug/msync plan <old-dir> ./my-org
```

An old directory with no `.msync/` is a first snapshot.

## For a real deployment

Dev mode is the opposite of a deployment. Change all of this:

1. Unset `MSYNC_DEV_MODE` and `MSYNC_CRA_LOCAL_DIR`.
2. Put the hierarchy in a repository. Set `MSYNC_CRA_REPO` (`provider:host:path`), `MSYNC_CRA_BRANCH`, and the read token (`GITHUB_TOKEN` for GitHub, `GITLAB_TOKEN` for GitLab). See `docs/CRA_CONFIG.md`.
3. Set owners in `MSYNC_OWNER_MEMBER_IDS` as `github:<login>` or `gitlab:<login>`. Only that form can bootstrap the first snapshot.
4. Connect at least one chat platform: Discord (`DISCORD_TOKEN`, `DISCORD_APPLICATION_ID`; `docs/DISCORD.md`) or Slack (`MSYNC_SLACK_SIGNING_SECRET`; `docs/SLACK.md`). Outside dev mode the server refuses to start without one.
5. Set webhook secrets: `GITHUB_WEBHOOK_SECRET` and `GITLAB_WEBHOOK_SECRET`. With no secret, every webhook for that host is refused.
6. Use a persistent `DATABASE_URL` on storage that is backed up. Back it up on a schedule and test a restore.
7. Put HTTPS in front. `nginx.conf` in the repository is a starting point.
8. Set `MSYNC_PUBLIC_URL` to the public address (OAuth redirects and Stripe return links depend on it).
9. Set `MSYNC_KEY_ENCRYPTION_KEY` (32 bytes as 64 hex characters) to enable MSync as an OAuth/OpenID Connect provider. Without it every `/oauth/*` endpoint answers 503.
10. Payments: start with Stripe test keys (`sk_test_...`). Live keys are refused unless `MSYNC_STRIPE_ALLOW_LIVE=true`; set that only deliberately. Leave `MSYNC_MCP_ALLOW_EXECUTE` unset.
11. Run the doctor before you start the service:

```bash
msync doctor
```

It lists each connector as configured or not and names every missing variable. It never prints a secret value.

12. Install the CI workflow from the template (`.github/workflows/msync-cra.yml`, or `.msync/gitlab-ci.yml` for GitLab) in the hierarchy repository. It runs `msync check` and `msync plan` on every pull or merge request that touches `.msync/` and posts the preview. It downloads the binary from the variable `MSYNC_RELEASE_URL`; no release is published yet, so set that variable yourself or use the commented `cargo install` alternative in the file.

## Next

- `docs/SETUP_WITH_AN_ASSISTANT.md`: let an AI assistant do this with you.
- `AGENTS.md`: the instructions the assistant follows.
- `docs/CRA_PLATFORM.md`: the full contract.
