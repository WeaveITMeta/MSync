-- Payroll control (contract section 11): payees, pay orders, invoices and ledger sources.
-- Additive only. payout_ledger keeps UNIQUE (pr_id, merge_sha) and the dsync-ledger-<id>
-- transfer idempotency key; it only gains four columns and one partial unique index.
--
-- Times are 'YYYY-MM-DD HH:MM:SS' UTC (auth::tokens::format_db_time) unless noted.
-- Money is integer cents. Percentages are basis points (10000 = 100%).

-- ------------------------------------------------------------------ payees
-- A person or company that can be paid. There is deliberately no column for a bank account
-- or card number: the payout account is the payee's Stripe Connect account.
CREATE TABLE IF NOT EXISTS payees (
    id TEXT PRIMARY KEY NOT NULL,
    org_id TEXT NOT NULL DEFAULT 'default',
    display_name TEXT NOT NULL,
    legal_name TEXT,
    kind TEXT NOT NULL CHECK (kind IN ('individual', 'company')),
    email TEXT,
    member_id TEXT,                               -- optional CRA member id
    stripe_account_id TEXT,                       -- Stripe Connect account (acct_...)
    stripe_payouts_enabled INTEGER NOT NULL DEFAULT 0 CHECK (stripe_payouts_enabled IN (0, 1)),
    tax_form_status TEXT NOT NULL DEFAULT 'none' CHECK (tax_form_status IN ('none', 'requested', 'on_file')),
    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'paused', 'closed')),
    notes TEXT,
    created_by TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_by TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_payees_member ON payees(member_id);
CREATE INDEX IF NOT EXISTS idx_payees_stripe ON payees(stripe_account_id);

-- The GitHub or GitLab logins (ledger payee keys, lowercased) a payee is paid under.
-- One payee per key.
CREATE TABLE IF NOT EXISTS payee_keys (
    payee_key TEXT PRIMARY KEY NOT NULL,
    payee_id TEXT NOT NULL REFERENCES payees(id),
    org_id TEXT NOT NULL DEFAULT 'default'
);
CREATE INDEX IF NOT EXISTS idx_payee_keys_payee ON payee_keys(payee_id);

-- Every change to a payee: who and when, and which fields changed.
CREATE TABLE IF NOT EXISTS payee_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    payee_id TEXT NOT NULL,
    action TEXT NOT NULL,                         -- create | update
    actor TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT '{}',            -- JSON: changed field names
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_payee_events_payee ON payee_events(payee_id, id);

-- ------------------------------------------------------------------ pay orders
CREATE TABLE IF NOT EXISTS pay_orders (
    id TEXT PRIMARY KEY NOT NULL,
    org_id TEXT NOT NULL DEFAULT 'default',
    name TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('draft', 'active', 'paused', 'ended', 'cancelled')),
    currency TEXT NOT NULL,
    amount_rule TEXT NOT NULL,                    -- JSON (payorders::AmountRule)
    schedule TEXT NOT NULL,                       -- JSON (payorders::OrderSchedule)
    memo TEXT,
    -- Bumped by every change to the amount rule, payees, schedule or currency. Approvals attach
    -- to one revision, so a change clears them.
    revision INTEGER NOT NULL DEFAULT 1,
    revised_by TEXT NOT NULL,                     -- author of the current revision
    -- Set on resume: occurrences dated before it were skipped while paused and never paid.
    resume_from TEXT,
    created_by TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_by TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_pay_orders_status ON pay_orders(status);

CREATE TABLE IF NOT EXISTS pay_order_approvals (
    order_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    approver_member_id TEXT NOT NULL,
    org_id TEXT NOT NULL DEFAULT 'default',
    token_id TEXT,
    created_at TEXT NOT NULL,
    UNIQUE (order_id, revision, approver_member_id)
);

CREATE TABLE IF NOT EXISTS pay_order_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    order_id TEXT NOT NULL,
    action TEXT NOT NULL,                         -- create | update | approve | pause | resume | cancel | end
    from_status TEXT,
    to_status TEXT,
    revision INTEGER NOT NULL,
    actor TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_pay_order_events_order ON pay_order_events(order_id, id);

-- One row per schedule occurrence, written once: UNIQUE (order_id, period_key) is what makes a
-- restart or a double tick unable to pay twice. period_key is the occurrence date (YYYY-MM-DD).
CREATE TABLE IF NOT EXISTS pay_order_occurrences (
    id TEXT PRIMARY KEY NOT NULL,
    order_id TEXT NOT NULL,
    org_id TEXT NOT NULL DEFAULT 'default',
    period_key TEXT NOT NULL,
    occurs_at TEXT NOT NULL,
    period_start TEXT NOT NULL,
    period_end TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('materialized', 'skipped')),
    order_revision INTEGER NOT NULL,
    pool_cents INTEGER NOT NULL,                  -- amount the rule distributed (0 when skipped)
    paid_cents INTEGER NOT NULL,                  -- sum of the ledger rows written
    leftover_cents INTEGER NOT NULL,              -- not paid, never given to anyone else
    detail TEXT NOT NULL,                         -- JSON: shares and the reason for each unpaid amount
    created_at TEXT NOT NULL,
    UNIQUE (order_id, period_key)
);

-- ------------------------------------------------------------------ invoices
CREATE TABLE IF NOT EXISTS invoices (
    id TEXT PRIMARY KEY NOT NULL,
    org_id TEXT NOT NULL DEFAULT 'default',
    payee_id TEXT NOT NULL REFERENCES payees(id),
    number TEXT NOT NULL COLLATE NOCASE,
    issue_date TEXT NOT NULL,                     -- YYYY-MM-DD
    due_date TEXT NOT NULL,                       -- YYYY-MM-DD
    currency TEXT NOT NULL,
    lines TEXT NOT NULL,                          -- JSON array of {description, quantity, unit_amount_cents}
    subtotal_cents INTEGER NOT NULL,
    tax_cents INTEGER NOT NULL DEFAULT 0,
    total_cents INTEGER NOT NULL,
    memo TEXT,
    attachment_ref TEXT,
    status TEXT NOT NULL CHECK (status IN ('submitted', 'approved', 'rejected', 'scheduled', 'paid', 'void')),
    ledger_id TEXT,                               -- the one ledger row an approved invoice becomes
    submitted_by TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    CHECK (total_cents = subtotal_cents + tax_cents),
    UNIQUE (payee_id, number)
);
CREATE INDEX IF NOT EXISTS idx_invoices_status ON invoices(status);

CREATE TABLE IF NOT EXISTS invoice_approvals (
    invoice_id TEXT NOT NULL,
    approver_member_id TEXT NOT NULL,
    org_id TEXT NOT NULL DEFAULT 'default',
    token_id TEXT,
    created_at TEXT NOT NULL,
    UNIQUE (invoice_id, approver_member_id)
);

-- Every invoice state change and approval: who and when.
CREATE TABLE IF NOT EXISTS invoice_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    invoice_id TEXT NOT NULL,
    action TEXT NOT NULL,                         -- submit | approval | approve | schedule | reject | void | paid
    from_status TEXT,
    to_status TEXT NOT NULL,
    actor TEXT NOT NULL,
    note TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_invoice_events_invoice ON invoice_events(invoice_id, id);

-- ------------------------------------------------------------------ ledger sources
-- Existing rows become 'merit'. Non-merit rows fill the legacy NOT NULL columns so they can
-- never collide with a real merge (see payments::ledger::insert_source_row):
--   pr_id      = -1 (pay_order) or -2 (invoice); real pull/merge request ids are positive
--   pr_number  = 0
--   merge_sha  = 'pay_order:<occurrence id>:<payee id>' or 'invoice:<invoice id>'; a real merge
--                sha is hexadecimal and never contains ':'
--   branch     = the source kind; base_amount_cents = amount_cents; merit_points = 0
--   github_username = the payee's first payee key, or 'payee:<payee id>' when it has none
ALTER TABLE payout_ledger ADD COLUMN source_kind TEXT NOT NULL DEFAULT 'merit'
    CHECK (source_kind IN ('merit', 'pay_order', 'invoice'));
ALTER TABLE payout_ledger ADD COLUMN source_ref TEXT;     -- occurrence id or invoice id
ALTER TABLE payout_ledger ADD COLUMN payee_id TEXT;       -- payees.id for non-merit rows
ALTER TABLE payout_ledger ADD COLUMN pay_after TEXT;      -- not payable before this time

CREATE UNIQUE INDEX IF NOT EXISTS idx_payout_ledger_source
    ON payout_ledger(source_kind, source_ref, payee_id) WHERE source_kind <> 'merit';
CREATE INDEX IF NOT EXISTS idx_payout_ledger_payee ON payout_ledger(payee_id);
