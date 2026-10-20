-- Keys of webhook deliveries / effects already applied, so duplicate GitHub
-- deliveries never award merit twice.
CREATE TABLE IF NOT EXISTS processed_webhook_events (
    event_key TEXT PRIMARY KEY NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);