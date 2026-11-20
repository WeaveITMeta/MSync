-- Chat platforms: Discord and Slack side by side (contract 12.2).
-- Additive only: no existing table or column is changed or dropped.
-- contributors.discord_user_id stays; new lookups by chat user go through chat_identities.

-- One row per chat account bound to a contributor. One contributor may hold a Discord id
-- and a Slack id at the same time.
--   platform       discord | slack
--   team_or_guild  the Slack team id. Discord user ids are global, so Discord rows use ''.
--   user_id        the Discord snowflake or the Slack user id, as text
--   contributor_id the same value (and storage type) as contributors.id
--   verified       1 only when the binding is proven (a future OAuth link). A binding made
--                  by typing a login into `link` is 0, and so is every row backfilled here.
--   source         link | legacy
CREATE TABLE IF NOT EXISTS chat_identities (
    platform TEXT NOT NULL,
    team_or_guild TEXT NOT NULL DEFAULT '',
    user_id TEXT NOT NULL,
    contributor_id TEXT NOT NULL,
    verified INTEGER NOT NULL DEFAULT 0,
    source TEXT NOT NULL DEFAULT 'link',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (platform, team_or_guild, user_id)
);
-- A contributor holds at most one account per platform and team, so two chat users can
-- never both be bound to the same GitHub or GitLab login, even in a race.
CREATE UNIQUE INDEX IF NOT EXISTS idx_chat_identities_one_per_contributor
    ON chat_identities(platform, team_or_guild, contributor_id);

-- Every Discord binding made before this change came from the old unproven `/link`,
-- so it is backfilled as unverified. The old `/link` could point one Discord id at several
-- logins; only the oldest contributor row keeps it here (the others stay in
-- contributors.discord_user_id, unused by chat lookups).
INSERT OR IGNORE INTO chat_identities (platform, team_or_guild, user_id, contributor_id, verified, source)
SELECT 'discord', '', CAST(discord_user_id AS TEXT), id, 0, 'legacy'
FROM contributors
WHERE discord_user_id IS NOT NULL
ORDER BY created_at, id;
