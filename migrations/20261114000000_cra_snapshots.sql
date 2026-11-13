-- CRA snapshots (contract section 4): every attempt to load the organization's
-- `.dsync/` files at one commit. Additive only.
--
-- status: active | superseded | rejected. Rejected attempts are kept, never deleted.
-- retryable = 1 marks a rejection caused by the Foundry API (fetch or context error) or a
-- concurrent activation; the same commit may be attempted again. Every other outcome is
-- final for its (org, commit): loading the same commit twice does nothing the second time.
CREATE TABLE IF NOT EXISTS cra_snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    org_id TEXT NOT NULL,                     -- engine org (api_tokens.org_id), 'default' today
    org_slug TEXT,                            -- org.toml [org].slug; NULL when the files did not parse
    provider TEXT NOT NULL,                   -- github | gitlab
    host TEXT NOT NULL,
    repo TEXT NOT NULL,
    commit_sha TEXT NOT NULL,
    content_hash TEXT,                        -- cra::CraFiles::content_hash; NULL when not fetched
    org_toml TEXT,
    roles_toml TEXT,
    members_toml TEXT,
    merit_toml TEXT,
    payouts_toml TEXT,
    status TEXT NOT NULL CHECK (status IN ('active', 'superseded', 'rejected')),
    retryable INTEGER NOT NULL DEFAULT 0 CHECK (retryable IN (0, 1)),
    actor TEXT,                               -- provider:login that merged or pushed
    author TEXT,                              -- pull/merge request author, provider:login
    via_pull_request INTEGER NOT NULL DEFAULT 0,
    approvers TEXT NOT NULL DEFAULT '[]',     -- JSON array of provider:login
    violations TEXT NOT NULL DEFAULT '[]',    -- JSON array of {code, item, message}
    plan_markdown TEXT,
    loaded_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    superseded_at TEXT,
    CHECK (status = 'rejected' OR retryable = 0)
);

-- At most one active snapshot per org.
CREATE UNIQUE INDEX IF NOT EXISTS idx_cra_snapshots_one_active
    ON cra_snapshots(org_id) WHERE status = 'active';

-- One final outcome per (org, commit).
CREATE UNIQUE INDEX IF NOT EXISTS idx_cra_snapshots_final_commit
    ON cra_snapshots(org_id, commit_sha) WHERE retryable = 0;

CREATE INDEX IF NOT EXISTS idx_cra_snapshots_org_loaded
    ON cra_snapshots(org_id, id DESC);
