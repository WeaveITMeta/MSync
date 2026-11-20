-- Audit log (contract 13.1, hierarchy doc 7.2) and the portal sync API (contract 13.2,
-- hierarchy doc section 8). Additive only: no existing table or column is changed.

-- One row per audited event, hash-chained. `src/audit/mod.rs` documents the canonical
-- serialization that `hash` is computed over.
--   seq        1, 2, 3, ... with no gaps; assigned by the writer as head + 1
--   at         RFC 3339 UTC with milliseconds, e.g. 2026-11-21T19:00:00.000Z
--   actor_kind member | admin_token | system | foundry | chat | consumer
--   detail     canonical JSON (object keys sorted, no whitespace)
--   prev_hash  the previous entry's hash; 64 zeros for seq 1
CREATE TABLE IF NOT EXISTS audit_log (
    seq INTEGER PRIMARY KEY,
    at TEXT NOT NULL,
    org_id TEXT NOT NULL,
    actor_kind TEXT NOT NULL,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    target TEXT NOT NULL,
    detail TEXT NOT NULL,
    prev_hash TEXT NOT NULL UNIQUE,
    hash TEXT NOT NULL UNIQUE
);
CREATE INDEX IF NOT EXISTS idx_audit_log_action ON audit_log(action, seq);
CREATE INDEX IF NOT EXISTS idx_audit_log_actor ON audit_log(actor, seq);
CREATE INDEX IF NOT EXISTS idx_audit_log_target ON audit_log(target, seq);

-- Append-only: the database refuses every UPDATE and DELETE.
CREATE TRIGGER IF NOT EXISTS audit_log_no_update BEFORE UPDATE ON audit_log
BEGIN
    SELECT RAISE(ABORT, 'audit_log is append-only');
END;
CREATE TRIGGER IF NOT EXISTS audit_log_no_delete BEFORE DELETE ON audit_log
BEGIN
    SELECT RAISE(ABORT, 'audit_log is append-only');
END;
-- Every insert must extend the current head: the next seq, linked to the head's hash. With
-- prev_hash UNIQUE this makes a fork impossible even for a buggy writer.
CREATE TRIGGER IF NOT EXISTS audit_log_extends_head BEFORE INSERT ON audit_log
WHEN NEW.seq <> (SELECT COALESCE(MAX(seq), 0) + 1 FROM audit_log)
  OR NEW.prev_hash <> COALESCE(
        (SELECT hash FROM audit_log WHERE seq = NEW.seq - 1),
        '0000000000000000000000000000000000000000000000000000000000000000')
BEGIN
    SELECT RAISE(ABORT, 'audit_log entry does not extend the chain head');
END;

-- Replay protection for signed consumer requests: a nonce is accepted once per consumer.
-- Rows older than 10 minutes are swept (the timestamp window is 5 minutes).
CREATE TABLE IF NOT EXISTS consumer_nonces (
    consumer TEXT NOT NULL,
    nonce TEXT NOT NULL,
    seen_at INTEGER NOT NULL,
    UNIQUE (consumer, nonce)
);
CREATE INDEX IF NOT EXISTS idx_consumer_nonces_seen ON consumer_nonces(seen_at);

-- Every consumer lookup that passed authentication (hierarchy doc 8.6). `subject` is the
-- pairwise id when the member was found, NULL otherwise.
CREATE TABLE IF NOT EXISTS consumer_lookups (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    org_id TEXT NOT NULL,
    consumer TEXT NOT NULL,
    key_id TEXT NOT NULL,
    lookup TEXT NOT NULL,
    subject TEXT,
    result TEXT NOT NULL,
    scopes_requested TEXT NOT NULL,
    scopes_granted TEXT NOT NULL,
    at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_consumer_lookups_consumer_at ON consumer_lookups(consumer, at);
