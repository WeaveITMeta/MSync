# MSync on Slack

MSync runs on Slack and on Discord with the same eight commands. An organization can use Slack only, Discord only, or both at once. Neither platform is the source of truth: both show what the CRA (`.msync/` in your repository) says. This page sets up the Slack app. `docs/DISCORD.md` sets up the Discord bot.

## 1. Create the app from the manifest

1. Open `slack/manifest.yaml` and replace `https://msync.example.com` with the public URL of your MSync service in all eight commands. The path stays `/slack/commands`.
2. Go to https://api.slack.com/apps, choose **Create New App**, then **From an app manifest**, pick your workspace and paste the file.
3. The manifest asks for five bot scopes and nothing else:

| Scope | Why |
|---|---|
| `commands` | The eight slash commands. |
| `users:read` | `users.info`, to check whether the caller is a workspace admin. |
| `usergroups:read` | Read the user groups that carry job titles. |
| `usergroups:write` | Set the members of those user groups (`usergroups.users.update`). |
| `chat:write` | Post replies. |

## 2. Set the secrets

MSync reads three settings. Set them where MSync runs, then restart it.

| Variable | Where to find it | Required |
|---|---|---|
| `MSYNC_SLACK_SIGNING_SECRET` | App settings, **Basic Information**, **Signing Secret**. | Yes. Slack is on only when this is set. |
| `MSYNC_SLACK_BOT_TOKEN` | **OAuth & Permissions**, **Bot User OAuth Token** (`xoxb-...`), after you install the app. | For admin checks, role sync and alerts. Without it, Slack role sync is skipped. |
| `MSYNC_SLACK_TEAM_ID` | Your workspace id (`T...`). It is in the workspace URL and in `team.info`. | Unless your CRA's `org.toml` has `[slack] team_id`. |

Treat the signing secret and the bot token like passwords. Do not commit them.

MSync checks every request before it reads it. The `X-Slack-Signature` header must be the HMAC-SHA256 of `v0:<timestamp>:<body>` under the signing secret, and the timestamp must be within five minutes. With no signing secret, every request is refused. A request from any workspace other than `MSYNC_SLACK_TEAM_ID` or the CRA's `[slack] team_id` is refused.

## 3. Install

On **Install App**, install the app to your workspace and copy the bot token into `MSYNC_SLACK_BOT_TOKEN`. Restart MSync. The log line `Slack enabled: POST /slack/commands` confirms it. Type `/msync-leaderboard` in any channel to test.

## 4. The commands

Slack slash commands are shared by every app in a workspace, so MSync's start with `msync-`. Each one does exactly what its Discord twin does.

| Slack | Discord | What it does | Reply |
|---|---|---|---|
| `/msync-link <login>` | `/link` | Without a CRA: links your Slack account to a GitHub login (`alice`) or GitLab login (`gitlab:alice`). With a CRA: links nothing and shows what `members.toml` lists for you. | Only you |
| `/msync-stats` | `/stats` | Your merit points, contributions and current role. | Channel |
| `/msync-leaderboard` | `/leaderboard` | The top ten contributors. | Channel |
| `/msync-sync` | `/sync` | Applies the CRA job titles to Slack user groups. Admins only. | Channel |
| `/msync-payouts` | `/payouts` | Payouts waiting for approval. Admins only. Approval itself is in the admin API. | Only you |
| `/msync-payout-setup` | `/payout-setup` | Your private Stripe link to receive payouts. Verified members only. | Only you |
| `/msync-clock-in` | `/clock-in` | Clock in for your current shift; the shift sets the end. Verified members only. | Only you |
| `/msync-shift` | `/shift` | Your current or next shift. Verified members only. | Only you |

### Linking rules

- A login already linked to another Slack account is refused.
- Your own link never changes from chat. To change it, an administrator adds you to the CRA member list.
- A link you type is not verified. It shows your stats, but it cannot set up payouts. To get paid, ask an administrator to add you to `members.toml` with your GitHub or GitLab login and your Slack user id (`slack = "U..."`). Members listed there are verified.

### Who counts as an admin

- With a CRA: your CRA rank must be at or above `grants.two_person_min_rank` (50 in the `source_available` and `hybrid` modes, 30 in `closed`, 60 in `open`). The Slack admin flag does not count.
- Without a CRA: Slack must report you as a workspace admin (`users.info`, `is_admin`). If MSync cannot ask Slack (no bot token, an API error), the command is refused.

## 5. Job titles as user groups

Slack has no roles. A job title is shown as membership in a Slack user group. In `roles.toml`, give a role its user group id:

```toml
[[role]]
key = "maintainer"
title = "Maintainer"
kind = "position"
rank = 60
slack_usergroup = "S0123456789"
```

Create the user groups in Slack first (**People**, **User groups**), then copy each id into `roles.toml`. In `members.toml`, give each member their Slack user id (`slack = "U..."`; in Slack, open the profile, **More**, **Copy member ID**).

MSync keeps every managed user group equal to the CRA projection (section 6). Anyone in such a group who should not hold the title is removed from it. Slack does not allow an empty user group, so MSync never removes a group's last member; that change is reported as failed in the reply, and the group should be disabled in Slack or given a member in the CRA.

User groups need a paid Slack plan (Pro or above). On a free plan, `/msync-sync` reports each group as failed (`paid_teams_only`), and every other command works. Some workspaces also limit who may change user groups; if so, allow the MSync app to manage them.

## 6. Role sync and drift

Slack user groups are a projection of the CRA, never the truth. The rules are the same as on Discord (`docs/DISCORD.md`, section 5), written once and run for both platforms.

**What is synced.** Only the managed user groups: the `slack_usergroup` ids in `roles.toml`. Every other user group is never read or changed. A user group carries no permissions in Slack, so no user group is refused for its permissions.

**When.** On CRA snapshot activation (holds released), every `MSYNC_ROLE_SYNC_SECS` seconds (default 300; 0 turns the timer off), and on `/msync-sync` by an authorized member (holds released). A member who joins the workspace is picked up by the next timer run; MSync does not subscribe to Slack's `team_join` event yet. Slack sends no event when a user group is edited by hand, so on Slack drift is caught by the timer, not at once.

**Drift.** A managed user group changed by hand (someone added who should not be there, someone removed who should be, or anyone in it who is not a CRA member) is reverted, recorded in `chat_drift_events`, and posted to the channel named by `[slack] alert_channel_id` in `org.toml`. Invite the MSync app to that channel (`/invite @MSync`) so `chat:write` can post there.

**The hold (anti-flap).** After three reverts of the same user group on the same member within an hour, MSync stops reverting that pair, posts one alert, and keeps it held until the next snapshot activation or an explicit `/msync-sync` by an authorized member.

**Failures.** A change counts as applied only after Slack accepts it, and a failed change is retried on the next run. A rate limit (HTTP 429 or `ratelimited`) stops the run until Slack's `Retry-After` time has passed. Slack edits a user group by replacing its whole list, so MSync reads the list and writes it back for each change; a change made in Slack between the two can be overwritten, and is then caught as drift on a later run.

**The three-second rule.** Slack waits three seconds for a slash command's answer. `/msync-sync`, `/msync-payouts` and `/msync-payout-setup` call the Slack Web API or Stripe, so they answer at once with a short private "working" reply and post the result to the request's `response_url` when it is ready. MSync posts only to `https://hooks.slack.com/` URLs. The signature, the body and the workspace are checked before anything is acknowledged; a request that fails the check gets no reply and starts no work.

## 7. Discord and Slack side by side

- Set both platforms in `org.toml`: `[discord]` with the guild id and `[slack]` with the team id. At least one is required.
- A role can carry both `discord_role` and `slack_usergroup`. A member can carry both `discord` and `slack` identities. Changing a member's `slack` identity needs exactly the authority and the second approver that changing their `github` or `discord` identity needs.
- One person is one contributor. Their merit, stats and payout account are the same whether they ask on Discord or on Slack.
- MSync starts with Slack only (no `DISCORD_TOKEN`), Discord only (no `MSYNC_SLACK_SIGNING_SECRET`), or both. It refuses to start with neither.
- `examples/cra/eustress-engine` uses both platforms, `examples/cra/weave-solutions` uses Slack only, and `examples/cra/voltec-industries` uses Discord only.
