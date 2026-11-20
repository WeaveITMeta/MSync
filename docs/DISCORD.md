# MSync on Discord

MSync runs on Discord and on Slack with the same six commands. An organization can use Discord only, Slack only, or both at once. Neither platform is the source of truth: both show what the CRA (`.msync/` in your repository) says. This page sets up the Discord bot. `docs/SLACK.md` sets up the Slack app.

## 1. Create the bot

1. Go to https://discord.com/developers/applications and choose **New Application**.
2. On **Bot**, create the bot, then turn on **Server Members Intent** and **Message Content Intent**.
3. On **OAuth2**, **URL Generator**, pick the scopes `bot` and `applications.commands` and the bot permissions **Manage Roles**, **Send Messages** and **Read Message History**. Open the URL and add the bot to your server.
4. In **Server Settings**, **Roles**, drag the MSync role above every role it should manage. Discord only lets a bot change roles below its own.

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

### Linking rules

- A login already linked to another Discord account is refused.
- Your own link never changes from chat. To change it, an administrator adds you to the CRA member list.
- A link you type is not verified. It shows your stats, but it cannot set up payouts. Links made before this rule existed are not verified either. To get paid, ask an administrator to add you to `members.toml` with your GitHub or GitLab login and your Discord user id (`discord = "..."`). Members listed there are verified.

### Who counts as an admin

- With a CRA: your CRA rank must be at or above `grants.two_person_min_rank`. The Discord Administrator permission does not count.
- Without a CRA: you need the Discord Administrator permission. If Discord does not report your permissions, the command is refused.

## 4. Job titles as roles

In `roles.toml`, give a role its Discord role id (`discord_role = "..."`; right-click the role, **Copy Role ID**). In `members.toml`, give each member their Discord user id.

With a CRA, `/sync` gives every member in `members.toml` the roles their job titles carry and removes the managed roles they should not hold. Members who are not in `members.toml` are not changed. Without a CRA, `/sync` assigns the merit roles from the `DISCORD_ROLE_ID_*` settings, as before.

## 5. Discord and Slack side by side

See `docs/SLACK.md`, section 6. In short: set `[discord]` and `[slack]` in `org.toml`, give roles both `discord_role` and `slack_usergroup`, and give members both `discord` and `slack` identities. One person is one contributor on both platforms.
