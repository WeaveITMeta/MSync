//! The append-only, hash-chained audit log (contract 13.1, hierarchy doc 7.2).
//!
//! Every entry is one row of `audit_log`: `seq` (1, 2, 3, ... with no gaps), `at`, the org,
//! who acted (`actor_kind`, `actor`), what they did (`action`), to what (`target`), a JSON
//! `detail`, the previous entry's hash and this entry's hash. Database triggers refuse every
//! UPDATE and DELETE, and refuse an INSERT that does not extend the current head, so the chain
//! cannot fork even under concurrent writers.
//!
//! # Canonical serialization (stable; changing it breaks every stored chain)
//!
//! `hash` is the lowercase hex SHA-256 of the UTF-8 bytes of the canonical JSON of this array:
//!
//! ```text
//! ["msync-audit-v1", seq, at, org_id, actor_kind, actor, action, target, detail, prev_hash]
//! ```
//!
//! - `seq` is a JSON integer; every other scalar is a JSON string; `detail` is the JSON value.
//! - Canonical JSON: no whitespace anywhere; object keys sorted by their UTF-8 bytes; strings
//!   escaped exactly as `serde_json` escapes them (`"` `\` and control characters, nothing
//!   else); integers in plain decimal; floats in `serde_json`'s shortest round-trip form.
//! - `at` is RFC 3339 UTC with milliseconds (`2026-11-21T19:00:00.000Z`), hashed as stored.
//! - `prev_hash` is the previous entry's `hash`, or 64 `0` characters for `seq = 1`.
//! - The `detail` column stores exactly the canonical JSON of `detail`. [`verify`] treats a
//!   stored detail that is not in canonical form as broken.
//!
//! # What never goes in an entry
//!
//! Secrets, API tokens and full Stripe account ids. [`append`] and [`append_in`] pass the
//! actor, target and every string in `detail` through [`redact_str`], and replace the value of
//! any detail key named like a secret, before hashing. Call sites also pass
//! [`stripe_last4`] values themselves; redaction is the backstop, not the plan.
//!
//! # Limits, stated
//!
//! Someone with raw write access to the database file can drop the triggers and rewrite the
//! whole chain consistently, or remove entries from the end. The chain detects edits and
//! removals in the middle; anchoring the head outside the database (hierarchy doc 7.2) is what
//! catches a rewrite, and is not built yet.

pub mod api;

#[cfg(test)]
mod tests;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqliteConnection, SqlitePool};

use crate::auth::{Principal, DEFAULT_ORG, LEGACY_ADMIN_MEMBER};

/// The first element of the hashed array. A new serialization gets a new version string.
pub const HASH_VERSION: &str = "msync-audit-v1";

/// `prev_hash` of the first entry.
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// How many times [`append`] retries a write that lost a race or found the database busy.
const APPEND_ATTEMPTS: u32 = 40;

/// A deliberate defect for the mutation runs (`MSYNC_MUTANT=<name>`). Test builds only.
#[cfg(test)]
pub(crate) fn mutant(name: &str) -> bool {
    std::env::var("MSYNC_MUTANT")
        .map(|v| v == name)
        .unwrap_or(false)
}
#[cfg(not(test))]
#[inline(always)]
pub(crate) fn mutant(_name: &str) -> bool {
    false
}

// ------------------------------------------------------------------ entries

/// Who acted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    /// A token holder: the token's member.
    Member,
    /// The legacy `MSYNC_ADMIN_TOKEN` (`admin-token`).
    AdminToken,
    /// The engine itself (`system:<name>`): the scheduler, a reload.
    System,
    /// A GitHub or GitLab identity from a verified webhook (a CRA pusher).
    Foundry,
    /// A Discord or Slack account.
    Chat,
    /// A portal consumer authenticated by HMAC.
    Consumer,
}

impl ActorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ActorKind::Member => "member",
            ActorKind::AdminToken => "admin_token",
            ActorKind::System => "system",
            ActorKind::Foundry => "foundry",
            ActorKind::Chat => "chat",
            ActorKind::Consumer => "consumer",
        }
    }
}

/// The actor kind of a member id as the payment code records it (`decided_by`, `actor`).
pub fn kind_for_member(member: &str) -> ActorKind {
    if member == LEGACY_ADMIN_MEMBER {
        ActorKind::AdminToken
    } else if member.starts_with(crate::auth::tokens::SYSTEM_MEMBER_PREFIX) {
        ActorKind::System
    } else {
        ActorKind::Member
    }
}

/// One event to append.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditEntry {
    pub org_id: String,
    pub actor_kind: ActorKind,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub detail: Value,
}

impl AuditEntry {
    pub fn new(
        actor_kind: ActorKind,
        actor: impl Into<String>,
        action: impl Into<String>,
        target: impl Into<String>,
        detail: Value,
    ) -> Self {
        AuditEntry {
            org_id: DEFAULT_ORG.to_string(),
            actor_kind,
            actor: actor.into(),
            action: action.into(),
            target: target.into(),
            detail,
        }
    }

    /// An action taken with an API token. The actor is the token's member; the token id (never
    /// the token) is added to the detail.
    pub fn by_principal(
        p: &Principal,
        action: impl Into<String>,
        target: impl Into<String>,
        detail: Value,
    ) -> Self {
        let kind = kind_for_member(&p.member_id);
        let mut detail = detail;
        if let Value::Object(m) = &mut detail {
            m.insert("token_id".into(), Value::String(p.token_id.clone()));
        }
        let mut e = AuditEntry::new(kind, p.member_id.clone(), action, target, detail);
        e.org_id = p.org_id.clone();
        e
    }
}

/// A stored entry.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AuditRecord {
    pub seq: i64,
    pub at: String,
    pub org_id: String,
    pub actor_kind: String,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub detail: Value,
    pub prev_hash: String,
    pub hash: String,
}

// ------------------------------------------------------------------ canonical form and hash

fn write_canonical(v: &Value, out: &mut String) {
    match v {
        Value::Null | Value::Bool(_) | Value::Number(_) => out.push_str(&v.to_string()),
        Value::String(s) => out.push_str(&Value::String(s.clone()).to_string()),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(x, out);
            }
            out.push(']');
        }
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                write_canonical(&m[k], out);
            }
            out.push('}');
        }
    }
}

/// The canonical JSON of `v` (see the module documentation).
pub fn canonical_json(v: &Value) -> String {
    let mut s = String::new();
    write_canonical(v, &mut s);
    s
}

/// The hash of one entry over its canonical fields and the previous hash.
#[allow(clippy::too_many_arguments)]
pub fn entry_hash(
    seq: i64,
    at: &str,
    org_id: &str,
    actor_kind: &str,
    actor: &str,
    action: &str,
    target: &str,
    detail: &Value,
    prev_hash: &str,
) -> String {
    let arr = Value::Array(vec![
        Value::String(HASH_VERSION.into()),
        Value::from(seq),
        Value::String(at.into()),
        Value::String(org_id.into()),
        Value::String(actor_kind.into()),
        Value::String(actor.into()),
        Value::String(action.into()),
        Value::String(target.into()),
        detail.clone(),
        Value::String(prev_hash.into()),
    ]);
    hex::encode(Sha256::digest(canonical_json(&arr).as_bytes()))
}

/// Entry time: RFC 3339 UTC with milliseconds.
pub fn format_at(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

// ------------------------------------------------------------------ redaction

/// A Stripe account id as an audit entry may show it: `acct_...` plus its last four
/// characters. Anything else is passed through [`redact_str`].
pub fn stripe_last4(account: &str) -> String {
    let a = account.trim();
    match a.strip_prefix("acct_") {
        Some(rest) if rest.chars().count() > 4 => {
            let tail: String = rest
                .chars()
                .rev()
                .take(4)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            format!("acct_...{tail}")
        }
        Some(_) => "acct_...".to_string(),
        None => redact_str(a),
    }
}

fn is_alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

/// Prefixes whose whole token is a secret: Stripe secret and restricted keys, webhook signing
/// secrets, Slack tokens.
const SECRET_PREFIXES: &[&str] = &[
    "sk_live_", "sk_test_", "rk_live_", "rk_test_", "whsec_", "xoxb-", "xoxp-", "xoxa-", "xapp-",
];

/// Detail keys whose value is never stored.
const SECRET_KEYS: &[&str] = &[
    "secret",
    "password",
    "token",
    "api_key",
    "signature",
    "authorization",
    "client_secret",
    "signing_secret",
    "access_token",
    "refresh_token",
];

/// Replace secrets in free text: Stripe account ids keep their last four characters; Stripe
/// and Slack secrets and the secret half of an MSync API token are replaced; a bearer value is
/// replaced. Idempotent.
pub fn redact_str(s: &str) -> String {
    if mutant("audit_no_redaction") {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    'outer: while i < b.len() {
        // Only look for a pattern at a token boundary.
        let boundary = i == 0 || !is_alnum(b[i - 1]);
        if boundary {
            let rest = &s[i..];
            if let Some(after) = rest.strip_prefix("acct_") {
                let n = after.bytes().take_while(|c| is_alnum(*c)).count();
                if n > 0 {
                    out.push_str(&stripe_last4(&rest[..5 + n]));
                    i += 5 + n;
                    continue;
                }
            }
            for p in SECRET_PREFIXES {
                if let Some(after) = rest.strip_prefix(p) {
                    let n = after
                        .bytes()
                        .take_while(|c| is_alnum(*c) || *c == b'-' || *c == b'_')
                        .count();
                    if n > 0 {
                        out.push_str("[redacted]");
                        i += p.len() + n;
                        continue 'outer;
                    }
                }
            }
            for p in ["msync_", "dsync_"] {
                if let Some(after) = rest.strip_prefix(p) {
                    let id = after.bytes().take_while(|c| c.is_ascii_hexdigit()).count();
                    if id > 0 && after.as_bytes().get(id) == Some(&b'_') {
                        let sec = after[id + 1..]
                            .bytes()
                            .take_while(|c| c.is_ascii_hexdigit())
                            .count();
                        if sec >= 8 {
                            out.push_str(p);
                            out.push_str(&after[..id]);
                            out.push_str("_[redacted]");
                            i += p.len() + id + 1 + sec;
                            continue 'outer;
                        }
                    }
                }
            }
            if rest
                .get(..7)
                .is_some_and(|p| p.eq_ignore_ascii_case("bearer "))
            {
                let n = rest[7..]
                    .bytes()
                    .take_while(|c| !c.is_ascii_whitespace() && *c != b'"')
                    .count();
                if n > 0 && !rest[7..].starts_with("[redacted]") {
                    out.push_str(&rest[..7]);
                    out.push_str("[redacted]");
                    i += 7 + n;
                    continue;
                }
            }
        }
        let ch = s[i..].chars().next().expect("in bounds");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// [`redact_str`] over every string in a JSON value, and the value of every secret-named key
/// replaced.
pub fn redact_value(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(redact_str(s)),
        Value::Array(a) => Value::Array(a.iter().map(redact_value).collect()),
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, x) in m {
                let secret = !mutant("audit_no_redaction")
                    && SECRET_KEYS.contains(&k.to_ascii_lowercase().as_str());
                out.insert(
                    redact_str(k),
                    if secret {
                        Value::String("[redacted]".into())
                    } else {
                        redact_value(x)
                    },
                );
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

// ------------------------------------------------------------------ append

fn is_retryable(e: &sqlx::Error) -> bool {
    match e {
        sqlx::Error::Database(d) => {
            let m = d.message().to_ascii_lowercase();
            m.contains("locked")
                || m.contains("busy")
                || m.contains("unique")
                || m.contains("does not extend the chain head")
        }
        sqlx::Error::PoolTimedOut => true,
        _ => false,
    }
}

/// Append one entry inside the caller's transaction (`&mut *tx`). The entry commits or rolls
/// back with the caller's change: no audit row, no change. Call it after the change's own
/// writes, so the transaction already holds the write lock and the head it reads is current.
pub async fn append_in(
    conn: &mut SqliteConnection,
    entry: &AuditEntry,
) -> Result<AuditRecord, sqlx::Error> {
    // Take the write lock before reading the head (a no-op UPDATE that touches no row and so
    // fires no trigger). Inside a transaction that already wrote, this changes nothing.
    if !mutant("audit_append_unlocked") {
        sqlx::query("UPDATE audit_log SET seq = seq WHERE 0")
            .execute(&mut *conn)
            .await?;
    }
    let head: Option<(i64, String)> =
        sqlx::query_as("SELECT seq, hash FROM audit_log ORDER BY seq DESC LIMIT 1")
            .fetch_optional(&mut *conn)
            .await?;
    let (seq, prev) = match head {
        Some((s, h)) => (s + 1, h),
        None => (1, GENESIS_HASH.to_string()),
    };
    let actor = redact_str(&entry.actor);
    let target = redact_str(&entry.target);
    let detail = redact_value(&entry.detail);
    let at = format_at(Utc::now());
    let kind = entry.actor_kind.as_str();
    let hash = entry_hash(
        seq,
        &at,
        &entry.org_id,
        kind,
        &actor,
        &entry.action,
        &target,
        &detail,
        &prev,
    );
    let inserted = sqlx::query(
        "INSERT INTO audit_log (seq, at, org_id, actor_kind, actor, action, target, detail, prev_hash, hash)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(seq)
    .bind(&at)
    .bind(&entry.org_id)
    .bind(kind)
    .bind(&actor)
    .bind(&entry.action)
    .bind(&target)
    .bind(canonical_json(&detail))
    .bind(&prev)
    .bind(&hash)
    .execute(&mut *conn)
    .await;
    match inserted {
        Ok(_) => {}
        // Mutant: treat the audit write as optional (what a strict call site must never do).
        Err(_) if mutant("audit_swallow_errors") => {}
        Err(e) => return Err(e),
    }
    Ok(AuditRecord {
        seq,
        at,
        org_id: entry.org_id.clone(),
        actor_kind: kind.to_string(),
        actor,
        action: entry.action.clone(),
        target,
        detail,
        prev_hash: prev,
        hash,
    })
}

/// Append one entry in its own transaction. Safe under concurrent writers: the write lock is
/// taken before the head is read, the database refuses any insert that does not extend the
/// head, and a write that lost a race or found the database busy is retried.
pub async fn append(pool: &SqlitePool, entry: &AuditEntry) -> Result<AuditRecord, sqlx::Error> {
    let mut last: Option<sqlx::Error> = None;
    for attempt in 0..APPEND_ATTEMPTS {
        let result = async {
            let mut tx = pool.begin().await?;
            let rec = append_in(&mut tx, entry).await?;
            tx.commit().await?;
            Ok::<AuditRecord, sqlx::Error>(rec)
        }
        .await;
        match result {
            Ok(rec) => return Ok(rec),
            Err(e) if is_retryable(&e) && !mutant("audit_append_unlocked") => {
                last = Some(e);
                // Spread retries out so writers that collided do not collide again.
                let jitter = u64::from(attempt % 7) * 3 + u64::from(attempt) * 2;
                tokio::time::sleep(std::time::Duration::from_millis(1 + jitter)).await;
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or(sqlx::Error::PoolTimedOut))
}

/// [`append`] for events whose action has already happened and must not be undone by a failed
/// audit write (a refused request, a transfer outcome). A failure is logged as an error.
pub async fn append_best_effort(pool: &SqlitePool, entry: &AuditEntry) {
    if let Err(e) = append(pool, entry).await {
        tracing::error!(
            "AUDIT WRITE FAILED for {} on {}: {}",
            entry.action,
            redact_str(&entry.target),
            e
        );
    }
}

// ------------------------------------------------------------------ read

fn record_from_row(r: &sqlx::sqlite::SqliteRow) -> Result<(AuditRecord, String), sqlx::Error> {
    let detail_text: String = r.try_get("detail")?;
    let detail = serde_json::from_str(&detail_text).unwrap_or(Value::Null);
    Ok((
        AuditRecord {
            seq: r.try_get("seq")?,
            at: r.try_get("at")?,
            org_id: r.try_get("org_id")?,
            actor_kind: r.try_get("actor_kind")?,
            actor: r.try_get("actor")?,
            action: r.try_get("action")?,
            target: r.try_get("target")?,
            detail,
            prev_hash: r.try_get("prev_hash")?,
            hash: r.try_get("hash")?,
        },
        detail_text,
    ))
}

/// Filters for [`list`]. `action` ending in `.` or `*` matches by prefix (`payout.*`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditFilter {
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    /// Only entries with `seq` greater than this (the paging cursor).
    #[serde(default)]
    pub after: Option<i64>,
    /// 1..=500, default 100.
    #[serde(default)]
    pub limit: Option<i64>,
}

pub const LIST_DEFAULT: i64 = 100;
pub const LIST_MAX: i64 = 500;

#[derive(Debug, Clone, Serialize)]
pub struct AuditPage {
    /// Oldest first.
    pub entries: Vec<AuditRecord>,
    /// Pass as `after` for the next page; None when this page is the last.
    pub next_after: Option<i64>,
}

/// Entries matching `f`, oldest first, one page.
pub async fn list(pool: &SqlitePool, f: &AuditFilter) -> Result<AuditPage, sqlx::Error> {
    let limit = f.limit.unwrap_or(LIST_DEFAULT).clamp(1, LIST_MAX);
    let mut sql = String::from("SELECT * FROM audit_log WHERE seq > ?");
    let action = f.action.as_deref().map(str::trim).filter(|a| !a.is_empty());
    let action_prefix = action.and_then(|a| {
        a.strip_suffix('*')
            .or_else(|| a.ends_with('.').then_some(a))
    });
    if f.actor.is_some() {
        sql.push_str(" AND actor = ?");
    }
    match (action, action_prefix) {
        (Some(_), Some(_)) => sql.push_str(" AND substr(action, 1, ?) = ?"),
        (Some(_), None) => sql.push_str(" AND action = ?"),
        _ => {}
    }
    if f.target.is_some() {
        sql.push_str(" AND target = ?");
    }
    sql.push_str(" ORDER BY seq ASC LIMIT ?");
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(f.after.unwrap_or(0));
    if let Some(a) = &f.actor {
        q = q.bind(a.clone());
    }
    match (action, action_prefix) {
        (Some(_), Some(p)) => q = q.bind(p.len() as i64).bind(p.to_string()),
        (Some(a), None) => q = q.bind(a.to_string()),
        _ => {}
    }
    if let Some(t) = &f.target {
        q = q.bind(t.clone());
    }
    let rows = q.bind(limit + 1).fetch_all(pool).await?;
    let mut entries = Vec::with_capacity(rows.len().min(limit as usize));
    for r in rows.iter().take(limit as usize) {
        entries.push(record_from_row(r)?.0);
    }
    let next_after = if rows.len() as i64 > limit {
        entries.last().map(|e| e.seq)
    } else {
        None
    };
    Ok(AuditPage {
        entries,
        next_after,
    })
}

// ------------------------------------------------------------------ verify

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrokenEntry {
    /// The first entry that does not verify.
    pub seq: i64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerifyReport {
    pub ok: bool,
    /// Entries that verified before the first broken one (all of them when `ok`).
    pub checked: i64,
    pub head_seq: Option<i64>,
    pub head_hash: Option<String>,
    pub first_broken: Option<BrokenEntry>,
}

/// Walk the whole chain from `seq = 1` and name the first entry that does not verify: a gap in
/// `seq` (a removed entry), a `prev_hash` that is not the previous entry's hash, a detail that
/// is not canonical, or a hash that does not match the contents.
pub async fn verify(pool: &SqlitePool) -> Result<VerifyReport, sqlx::Error> {
    const PAGE: i64 = 500;
    let mut expected_seq = 1i64;
    let mut prev = GENESIS_HASH.to_string();
    let mut checked = 0i64;
    let mut head: Option<(i64, String)> = None;
    let mut after = 0i64;
    loop {
        let rows = sqlx::query("SELECT * FROM audit_log WHERE seq > ? ORDER BY seq ASC LIMIT ?")
            .bind(after)
            .bind(PAGE)
            .fetch_all(pool)
            .await?;
        if rows.is_empty() {
            break;
        }
        for r in &rows {
            let (rec, detail_text) = record_from_row(r)?;
            let broken = |reason: String| VerifyReport {
                ok: false,
                checked,
                head_seq: head.as_ref().map(|h| h.0),
                head_hash: head.as_ref().map(|h| h.1.clone()),
                first_broken: Some(BrokenEntry {
                    seq: rec.seq,
                    reason,
                }),
            };
            if rec.seq != expected_seq && !mutant("audit_verify_skip_links") {
                return Ok(broken(format!(
                    "expected entry {expected_seq} next, found {}: an entry was removed",
                    rec.seq
                )));
            }
            if rec.prev_hash != prev && !mutant("audit_verify_skip_links") {
                return Ok(broken("prev_hash is not the previous entry's hash".into()));
            }
            if serde_json::from_str::<Value>(&detail_text).is_err()
                || canonical_json(&rec.detail) != detail_text
            {
                return Ok(broken("detail is not canonical JSON".into()));
            }
            let h = entry_hash(
                rec.seq,
                &rec.at,
                &rec.org_id,
                &rec.actor_kind,
                &rec.actor,
                &rec.action,
                &rec.target,
                &rec.detail,
                &rec.prev_hash,
            );
            if h != rec.hash && !mutant("audit_verify_skip_hash") {
                return Ok(broken("hash does not match the entry's contents".into()));
            }
            checked += 1;
            expected_seq += 1;
            prev = rec.hash.clone();
            head = Some((rec.seq, rec.hash.clone()));
            after = rec.seq;
        }
    }
    Ok(VerifyReport {
        ok: true,
        checked,
        head_seq: head.as_ref().map(|h| h.0),
        head_hash: head.map(|h| h.1),
        first_broken: None,
    })
}
