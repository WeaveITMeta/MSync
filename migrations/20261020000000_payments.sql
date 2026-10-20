-- Payment ledger: one row per merged PR (per merge commit) into a tracked branch.
CREATE TABLE IF NOT EXISTS payout_ledger (
    id TEXT PRIMARY KEY NOT NULL,
    pr_id INTEGER NOT NULL,              -- GitHub's global pull_request.id
    pr_number INTEGER NOT NULL,
    merge_sha TEXT NOT NULL,
    repo TEXT,
    title TEXT NOT NULL,
    github_username TEXT NOT NULL,
    contributor_id TEXT,
    branch TEXT NOT NULL,
    base_amount_cents INTEGER NOT NULL,
    merit_points INTEGER NOT NULL,
    amount_cents INTEGER NOT NULL,
    currency TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending_approval', -- pending_approval | approved | paid | failed | rejected
    stripe_transfer_id TEXT,
    failure_reason TEXT,
    decided_by TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (pr_id, merge_sha)
);

CREATE INDEX IF NOT EXISTS idx_payout_ledger_status ON payout_ledger(status);

-- Stripe Connect (Express) account per contributor
ALTER TABLE contributors ADD COLUMN stripe_account_id TEXT;
ALTER TABLE contributors ADD COLUMN stripe_payouts_enabled INTEGER NOT NULL DEFAULT 0;