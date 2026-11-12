-- API tokens, payout policy, approvals, blocks and runs (contract sections 3.5, 7, 8).
-- Additive only: payout_ledger is not altered.

-- Scoped API tokens: dsync_<id>_<secret>, only the SHA-256 hex of the full token is stored.
CREATE TABLE IF NOT EXISTS api_tokens (
    id TEXT PRIMARY KEY NOT NULL,
    org_id TEXT NOT NULL DEFAULT 'default',
    label TEXT NOT NULL,
    token_hash TEXT NOT NULL,
    scopes TEXT NOT NULL DEFAULT '[]',        -- JSON array of scope names
    member_id TEXT NOT NULL,
    payee_keys TEXT NOT NULL DEFAULT '[]',    -- JSON array of payout_ledger.github_username values (lowercased)
    created_by TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    expires_at TEXT,
    revoked_at TEXT,
    last_used_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_api_tokens_member ON api_tokens(member_id);

-- Versioned payout policy per org (the payouts.toml shape, stored as JSON).
CREATE TABLE IF NOT EXISTS payout_policies (
    org_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    policy_json TEXT NOT NULL,
    updated_by TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (org_id, version)
);

-- One approval per (ledger row, approver member). The approver is always the token's member.
CREATE TABLE IF NOT EXISTS payout_approvals (
    ledger_id TEXT NOT NULL,
    approver_member_id TEXT NOT NULL,
    org_id TEXT NOT NULL DEFAULT 'default',
    token_id TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (ledger_id, approver_member_id)
);
CREATE INDEX IF NOT EXISTS idx_payout_approvals_ledger ON payout_approvals(ledger_id);

-- Payees that must not be paid.
CREATE TABLE IF NOT EXISTS payout_blocks (
    payee_key TEXT PRIMARY KEY NOT NULL,      -- lowercased github_username
    org_id TEXT NOT NULL DEFAULT 'default',
    reason TEXT NOT NULL,
    created_by TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- A run is a set of ledger rows evaluated together. Scheduled runs carry a period_key;
-- manual runs leave it NULL (SQLite allows many NULLs under UNIQUE).
CREATE TABLE IF NOT EXISTS payout_runs (
    id TEXT PRIMARY KEY NOT NULL,
    org_id TEXT NOT NULL DEFAULT 'default',
    period_key TEXT,
    trigger_kind TEXT NOT NULL,               -- manual | scheduled
    status TEXT NOT NULL,                     -- prepared | completed | cancelled
    policy_version INTEGER,                   -- NULL = env default policy
    created_by TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    cancelled_by TEXT,
    cancelled_at TEXT,
    last_executed_by TEXT,
    last_executed_at TEXT,
    UNIQUE (org_id, period_key)
);

CREATE TABLE IF NOT EXISTS payout_run_items (
    run_id TEXT NOT NULL,
    ledger_id TEXT NOT NULL,
    org_id TEXT NOT NULL DEFAULT 'default',
    payee_key TEXT NOT NULL,
    amount_cents INTEGER NOT NULL,
    decision TEXT NOT NULL,                   -- include | skip (at preparation)
    reason TEXT,
    outcome TEXT,                             -- NULL | paid | failed | not_payable | already_paid
    outcome_reason TEXT,
    executed_at TEXT,
    PRIMARY KEY (run_id, ledger_id)
);
CREATE INDEX IF NOT EXISTS idx_payout_run_items_ledger ON payout_run_items(ledger_id);
