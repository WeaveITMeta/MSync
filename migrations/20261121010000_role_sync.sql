-- Role sync and drift (contract 13.3, hierarchy doc 7.3). Additive only.

-- A managed Discord role or Slack user group changed by hand: reverted, or held by the
-- anti-flap rule. `member_id` is the platform user id. `direction` is what the hand edit
-- did: 'granted' (MSync removes it) or 'removed' (MSync restores it). `reverted_at` is NULL
-- for a held pair.
CREATE TABLE IF NOT EXISTS chat_drift_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    platform TEXT NOT NULL CHECK (platform IN ('discord', 'slack')),
    role_id TEXT NOT NULL,
    member_id TEXT NOT NULL,
    direction TEXT NOT NULL CHECK (direction IN ('granted', 'removed')),
    outcome TEXT NOT NULL CHECK (outcome IN ('reverted', 'held')),
    observed_at TEXT NOT NULL,
    reverted_at TEXT,
    snapshot_id INTEGER,
    detail TEXT NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_chat_drift_events_pair
    ON chat_drift_events (platform, role_id, member_id, reverted_at);

-- Pairs MSync stopped reverting after three reverts within an hour. Open while
-- `released_at` is NULL; released by the next snapshot activation or an explicit sync.
CREATE TABLE IF NOT EXISTS chat_drift_holds (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    platform TEXT NOT NULL CHECK (platform IN ('discord', 'slack')),
    role_id TEXT NOT NULL,
    member_id TEXT NOT NULL,
    held_at TEXT NOT NULL,
    released_at TEXT,
    release_reason TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_chat_drift_holds_open
    ON chat_drift_holds (platform, role_id, member_id) WHERE released_at IS NULL;
