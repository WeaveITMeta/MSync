# MSync on Discord

MSync runs on Discord and on Slack with the same eight commands. An organization can use Discord only, Slack only, or both at once. Neither platform is the source of truth: both show what the CRA (`.msync/` in your repository) says. This page sets up the Discord bot. `docs/SLACK.md` sets up the Slack app.

## 1. Create the bot

1. Go to https://discord.com/developers/applications and choose **New Application**.
2. On **Bot**, create the bot, then turn on **Server Members Intent** and **Message Content Intent**.
3. On **OAuth2**, **URL Generator**, pick the scopes `bot` and `applications.commands` and the bot permissions **Manage Roles**, **Send Messages** and **Read Message History**. Open the URL and add the bot to your server.
4. In **Server Settings**, **Roles**, drag the MSync role above every role it should manage. Discord only lets a bot change roles below its own, and MSync refuses to manage a role at or above the bot's highest role (section 5).
5. Give the bot **Send Messages** in the channel named by `[discord] alert_channel_id` in `org.toml`; drift and refusals are reported there.

## 2. Set the secrets

| Variable | Where to find it | Required |
|---|---|---|
| `DISCORD_TOKEN` | **Bot**, **Reset Token**. | Yes. Discord is on only when this is set. |
| `DISCORD_APPLICATION_ID` | **General Information**, **Application ID**. | Yes, with `DISCORD_TOKEN`. |
| `DISCORD_GUILD_ID` | Your server id (turn on Developer Mode, right-click the server, **Copy Server ID**). | To register the slash commands. |

Treat the token like a password. Do not commit it. Restart MSync after setting them. The log line `Registered slash commands for guild ...` confirms it.

## 3. The commands

| Discord | Slack | What it does | Reply |
|---|---|---|---|
| `/link github:<login>` | `/msync-link` | Without a CRA: links your Discord account to a GitHub login (`alice`) or GitLab login (`gitlab:alice`). With a CRA: links nothing and shows what `members.toml` lists for you. | Only you |
| `/stats` | `/msync-stats` | Your merit points, contributions and current role. | Channel |
| `/leaderboard` | `/msync-leaderboard` | The top ten contributors. | Channel |
| `/sync` | `/msync-sync` | Applies the job titles to Discord roles. Admins only. | Channel |
| `/payouts` | `/msync-payouts` | Payouts waiting for approval. Admins only. Approval itself is in the admin API. | Only you |
| `/payout-setup` | `/msync-payout-setup` | Your private Stripe link to receive payouts. Verified members only. | Only you |
| `/clock-in` | `/msync-clock-in` | Clock in for your current shift; the shift sets the end. Verified members only. | Only you |
| `/shift` | `/msync-shift` | Your current or next shift. Verified members only. | Only you |

### Linking rules

- A login already linked to another Discord account is refused.
- Your own link never changes from chat. To change it, an administrator adds you to the CRA member list.
- A link you type is not verified. It shows your stats, but it cannot set up payouts. Links made before this rule existed are not verified either. To get paid, ask an administrator to add you to `members.toml` with your GitHub or GitLab login and your Discord user id (`discord = "..."`). Members listed there are verified.

### Who counts as an admin

- With a CRA: your CRA rank must be at or above `grants.two_person_min_rank`. The Discord Administrator permission does not count.
- Without a CRA: you need the Discord Administrator permission. If Discord does not report your permissions, the command is refused.

## 4. Job titles as roles

In `roles.toml`, give a role its Discord role id (`discord_role = "..."`; right-click the role, **Copy Role ID**). In `members.toml`, give each member their Discord user id.

With a CRA, MSync keeps every managed role equal to the CRA projection (section 5): members in `members.toml` get the roles their job titles carry, and anyone who holds a managed role without the CRA saying so loses it, including people who are not in `members.toml`. Without a CRA, `/sync` assigns the merit roles from the `DISCORD_ROLE_ID_*` settings, as before, and nothing runs on its own.

## 5. Role sync and drift

Discord roles are a projection of the CRA, never the truth. A role edited by hand never becomes truth: it is put back.

**What is synced.** Only the managed roles: the `discord_role` ids in `roles.toml`. Every other role in the server is never read, compared or changed. For each managed role MSync computes who should hold it from the active snapshot (job titles, tiers from current merit, grants that have not expired, suspended members hold nothing) and applies the difference.

**When.**

| Trigger | What happens |
|---|---|
| A CRA snapshot is activated | A full reconcile, and every hold (below) is released. |
| A member joins the server | A full reconcile, so the new member gets their roles at once. |
| A member's roles change (`GUILD_MEMBER_UPDATE`) | A full reconcile, so a hand edit is reverted within seconds. MSync's own changes cause these events too; the run they cause finds nothing to do. |
| Every `MSYNC_ROLE_SYNC_SECS` seconds (default 300; 0 turns the timer off) | A full reconcile. This also catches anything missed while MSync was down. |
| `/sync` by an authorized member | A full reconcile, holds released, and the result in the reply. |

Runs never overlap: one runner per platform, one lock, and triggers that arrive during a run are merged into one more run.

**Drift.** A managed role that differs from the projection because someone changed it by hand is drift: a role granted by hand to someone the CRA does not give it to, a role removed by hand from someone the CRA gives it to, or a managed role held by anyone who is not a CRA member at all. MSync reverts it, records it in the `chat_drift_events` table (platform, role, member, direction, observed and reverted times), and posts it to the alert channel. A difference caused by the CRA itself (a new grant, an expired grant, a suspension, a merit change) is applied without an alert.

**The hold (anti-flap).** MSync does not fight an edit war. After three reverts of the same role on the same member within an hour, it stops reverting that pair, posts one alert that the pair is held, and records the hold in `chat_drift_holds`. The pair stays held until the next CRA snapshot activation or an explicit `/sync` by an authorized member; after a release, the count starts over. The audit trail of who made the hand edit is in Discord's own audit log; MSync does not read it yet.

**Refused roles.** A CRA role mapped to a Discord role that carries **Administrator**, **Manage Roles**, **Manage Server**, **Ban Members**, **Kick Members** or **Manage Webhooks** is not managed at all: MSync never reads, grants or removes it, posts the refusal to the alert channel once, and lists it in every `/sync` reply. Merit and job titles never confer authority over the server. Remove the permission from the role, or map a different role. The same refusal applies to a role at or above the bot's own highest role (Discord would reject the change anyway); move the MSync bot role above every managed role.

**Failures.** A change counts as applied only after Discord accepts it. A failed change is retried on the next run, and repeating a run is always safe. A rate limit stops the run; it resumes on the next run after Discord's retry time. With no `DISCORD_TOKEN`, or no guild (`[discord] guild_id` in `org.toml`, else `DISCORD_GUILD_ID`), Discord role sync is skipped without error.

**Stated limit.** A Discord member with Administrator can always change roles; MSync cannot prevent it. It guarantees the change is reverted, recorded and reported, and that it grants nothing in MSync.

## 6. Discord and Slack side by side

See `docs/SLACK.md`, section 7. In short: set `[discord]` and `[slack]` in `org.toml`, give roles both `discord_role` and `slack_usergroup`, and give members both `discord` and `slack` identities. One person is one contributor on both platforms.
