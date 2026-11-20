-- Hourly pay and time (contract section 14): wages, shifts, time entries and timesheet rows.
--
-- Times are 'YYYY-MM-DD HH:MM:SS' UTC (auth::tokens::format_db_time) unless noted. Money is
-- integer cents, time is whole minutes, multipliers are basis points (10000 = 1x).
--
-- One change here is not a plain addition: payout_ledger.source_kind carries a CHECK that lists
-- the allowed sources ('merit', 'pay_order', 'invoice'), and SQLite cannot widen a CHECK in
-- place. The ledger table is therefore rebuilt once, below, with 'timesheet' added to that list
-- and nothing else changed: every column, every row (with its rowid), the UNIQUE (pr_id,
-- merge_sha) key and every index are carried over exactly. No other table references
-- payout_ledger with a foreign key and it has no triggers.

-- ------------------------------------------------------------------ wages
-- A rate history per payee. A new rate is a new row; nothing is ever updated or deleted. The
-- rate in force on a day is the newest approved row with effective_from on or before that day
-- (payments::wages::rate_on). A row with too few approvals pays nothing.
CREATE TABLE IF NOT EXISTS payee_wages (
    id TEXT PRIMARY KEY NOT NULL,
    org_id TEXT NOT NULL DEFAULT 'default',
    payee_id TEXT NOT NULL REFERENCES payees(id),
    rate_cents_per_hour INTEGER NOT NULL CHECK (rate_cents_per_hour >= 0),
    currency TEXT NOT NULL,
    effective_from TEXT NOT NULL,                 -- YYYY-MM-DD, a day in the shift's time zone
    -- Optional overtime: minutes beyond this many in one week (Monday to Sunday) are paid at
    -- overtime_bps / 10000 times the rate. Both set, or neither.
    overtime_after_minutes INTEGER CHECK (overtime_after_minutes IS NULL OR overtime_after_minutes >= 0),
    overtime_bps INTEGER CHECK (overtime_bps IS NULL OR overtime_bps >= 10000),
    memo TEXT,
    set_by TEXT NOT NULL,                         -- never an approver of this row
    created_at TEXT NOT NULL,
    CHECK ((overtime_after_minutes IS NULL) = (overtime_bps IS NULL))
);
CREATE INDEX IF NOT EXISTS idx_payee_wages_payee ON payee_wages(payee_id, effective_from);

CREATE TABLE IF NOT EXISTS payee_wage_approvals (
    wage_id TEXT NOT NULL,
    approver_member_id TEXT NOT NULL,
    org_id TEXT NOT NULL DEFAULT 'default',
    token_id TEXT,
    created_at TEXT NOT NULL,
    UNIQUE (wage_id, approver_member_id)
);

-- ------------------------------------------------------------------ shifts
-- When someone is expected to work. kind 'weekly' repeats on a weekday, 'single' is one dated
-- shift, 'open' is an open-shift allowance: a clock-in outside every other shift starts an
-- entry of length_minutes. Local times are in time_zone (an IANA name such as America/Phoenix).
-- An end_time at or before start_time ends on the next day.
CREATE TABLE IF NOT EXISTS shifts (
    id TEXT PRIMARY KEY NOT NULL,
    org_id TEXT NOT NULL DEFAULT 'default',
    payee_id TEXT NOT NULL REFERENCES payees(id),
    kind TEXT NOT NULL CHECK (kind IN ('weekly', 'single', 'open')),
    weekday TEXT,                                 -- weekly: monday .. sunday
    date TEXT,                                    -- single: YYYY-MM-DD
    start_time TEXT,                              -- HH:MM (weekly, single)
    end_time TEXT,                                -- HH:MM (weekly, single)
    length_minutes INTEGER,                       -- open
    time_zone TEXT NOT NULL,
    valid_from TEXT,                              -- weekly, open: first local date (optional)
    valid_until TEXT,                             -- weekly, open: last local date (optional)
    early_minutes INTEGER NOT NULL DEFAULT 15 CHECK (early_minutes >= 0),
    -- later_of_clock_in_and_start (default): paid from the shift start or the clock-in,
    -- whichever is later. clock_in: an early clock-in is paid from the clock-in.
    paid_from TEXT NOT NULL DEFAULT 'later_of_clock_in_and_start'
        CHECK (paid_from IN ('later_of_clock_in_and_start', 'clock_in')),
    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'ended')),
    created_by TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_by TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_shifts_payee ON shifts(payee_id, status);

-- ------------------------------------------------------------------ time entries
-- One entry per shift occurrence: UNIQUE (occurrence_key) is what makes a second clock-in
-- return the same entry. The key is '<shift id>:<local date>' for a weekly or single shift and
-- 'open:<shift id>:<start>' for an open-shift entry. start_at and end_at are whole minutes;
-- end_at is the shift's end, fixed at clock-in. Stored status 'open' reads as 'closed' once
-- end_at has passed (payments::timeclock::entry_state).
CREATE TABLE IF NOT EXISTS time_entries (
    id TEXT PRIMARY KEY NOT NULL,
    org_id TEXT NOT NULL DEFAULT 'default',
    payee_id TEXT NOT NULL REFERENCES payees(id),
    shift_id TEXT NOT NULL,
    occurrence_key TEXT NOT NULL UNIQUE,
    time_zone TEXT NOT NULL,
    clock_in_at TEXT NOT NULL,                    -- the exact clock-in time
    shift_start_at TEXT NOT NULL,
    shift_end_at TEXT NOT NULL,
    start_at TEXT NOT NULL,
    end_at TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('open', 'approved', 'void')),
    clocked_out_at TEXT,
    approved_by TEXT,
    approved_at TEXT,
    void_reason TEXT,
    clocked_in_by TEXT NOT NULL,                  -- the token member or chat account
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    CHECK (end_at >= start_at)
);
CREATE INDEX IF NOT EXISTS idx_time_entries_payee ON time_entries(payee_id, start_at);
CREATE INDEX IF NOT EXISTS idx_time_entries_status ON time_entries(status, end_at);

-- Every change to an entry: who, when, before and after, and the reason.
CREATE TABLE IF NOT EXISTS time_entry_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    entry_id TEXT NOT NULL,
    action TEXT NOT NULL,                         -- clock_in | clock_out | adjust | void | approve | auto_approve
    actor TEXT NOT NULL,
    reason TEXT,
    detail TEXT NOT NULL DEFAULT '{}',            -- JSON
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_time_entry_events_entry ON time_entry_events(entry_id, id);

-- ------------------------------------------------------------------ timesheet rows
-- One row per payee per pay period, written once (seq 0), plus one correction row (seq 1, 2,
-- ...) whenever time approved later raises what the period owes. A row never changes after it
-- is written. ledger_id is NULL when the row pays nothing (no approved rate, no minutes).
CREATE TABLE IF NOT EXISTS timesheet_rows (
    id TEXT PRIMARY KEY NOT NULL,
    org_id TEXT NOT NULL DEFAULT 'default',
    payee_id TEXT NOT NULL,
    period_key TEXT NOT NULL,
    period_start TEXT NOT NULL,
    period_end TEXT NOT NULL,
    seq INTEGER NOT NULL CHECK (seq >= 0),
    minutes INTEGER NOT NULL,                     -- approved minutes in the period when written
    owed_cents INTEGER NOT NULL,                  -- what the whole period owed when written
    amount_cents INTEGER NOT NULL CHECK (amount_cents >= 0),  -- this row: owed minus earlier rows
    ledger_id TEXT,
    detail TEXT NOT NULL,                         -- JSON: minutes per day, rates, leftover fraction
    created_at TEXT NOT NULL,
    UNIQUE (payee_id, period_key, seq)
);

-- ------------------------------------------------------------------ ledger: the timesheet source
-- Non-merit rows of source 'timesheet' fill the legacy NOT NULL columns like the other sources
-- (payments::ledger::legacy_columns): pr_id = -3, merge_sha = 'timesheet:<payee>:<period>' (or
-- 'timesheet:<payee>:<period>:<n>' for correction n). The partial unique index on (source_kind,
-- source_ref, payee_id) covers them as it covers pay orders and invoices.
CREATE TABLE payout_ledger_v2 (
    id TEXT PRIMARY KEY NOT NULL,
    pr_id INTEGER NOT NULL,
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
    status TEXT NOT NULL DEFAULT 'pending_approval',
    stripe_transfer_id TEXT,
    failure_reason TEXT,
    decided_by TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    source_kind TEXT NOT NULL DEFAULT 'merit'
        CHECK (source_kind IN ('merit', 'pay_order', 'invoice', 'timesheet')),
    source_ref TEXT,
    payee_id TEXT,
    pay_after TEXT,
    UNIQUE (pr_id, merge_sha)
);
INSERT INTO payout_ledger_v2 (rowid, id, pr_id, pr_number, merge_sha, repo, title, github_username, contributor_id,
    branch, base_amount_cents, merit_points, amount_cents, currency, status, stripe_transfer_id, failure_reason,
    decided_by, created_at, updated_at, source_kind, source_ref, payee_id, pay_after)
SELECT rowid, id, pr_id, pr_number, merge_sha, repo, title, github_username, contributor_id,
    branch, base_amount_cents, merit_points, amount_cents, currency, status, stripe_transfer_id, failure_reason,
    decided_by, created_at, updated_at, source_kind, source_ref, payee_id, pay_after
FROM payout_ledger;
DROP TABLE payout_ledger;
ALTER TABLE payout_ledger_v2 RENAME TO payout_ledger;
CREATE INDEX IF NOT EXISTS idx_payout_ledger_status ON payout_ledger(status);
CREATE UNIQUE INDEX IF NOT EXISTS idx_payout_ledger_source
    ON payout_ledger(source_kind, source_ref, payee_id) WHERE source_kind <> 'merit';
CREATE INDEX IF NOT EXISTS idx_payout_ledger_payee ON payout_ledger(payee_id);
