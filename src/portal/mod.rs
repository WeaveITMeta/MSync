//! Portal sync API (contract 13.2, hierarchy doc section 8): signed server-to-server reads of
//! one member's standing, for the consumers the active CRA lists in `org.toml` `[[consumer]]`.
//!
//! ```text
//! GET /api/v1/members/by-discord/{discord_id}?scopes=...
//! GET /api/v1/members/by-slack/{team_id}/{slack_user_id}?scopes=...
//! GET /api/v1/members/{subject}?scopes=...
//! ```
//!
//! There is no list or search endpoint. Every request carries `X-MSync-Consumer`,
//! `X-MSync-Key-Id`, `X-MSync-Timestamp`, `X-MSync-Nonce` and `X-MSync-Signature`
//! (`v1=<hex HMAC-SHA256>` over [`string_to_sign`]). The server checks, in this order, and every
//! failure is the same `401 {"error":"unauthorized"}`:
//!
//! 1. the consumer is in the active CRA and the key id has a secret
//!    (`MSYNC_CONSUMER_SECRET_<KEY>_<KEYID>`, at least 32 bytes, never stored);
//! 2. `|now - timestamp| <= 300` seconds;
//! 3. the signature, compared in constant time;
//! 4. the nonce is new for this consumer (`consumer_nonces`, swept after 10 minutes);
//! 5. the per-consumer rate limit (default 60 requests a minute).
//!
//! Unknown member, a member this consumer cannot see (another consumer's subject id, a Slack
//! team the CRA does not name) and a member without the looked-up identity all get the same
//! `404 {"error":"not_found"}`; so does every lookup while no CRA snapshot is active. A
//! suspended or removed member gets `200` with that status and an empty permission list.
//! Fields outside the consumer's allowlist are omitted, never null. Every authenticated
//! lookup is recorded in `consumer_lookups` and audited (`portal.lookup`); if that write fails
//! the request fails (503) rather than answer unrecorded. The full recipe is in
//! `docs/PORTAL_API.md`.

#[cfg(test)]
mod tests;

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use actix_web::{web, HttpRequest, HttpResponse};
use chrono::{NaiveDate, SecondsFormat, Utc};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tracing::{error, warn};

use crate::audit::{self, ActorKind, AuditEntry};
use crate::cra::loader::{ActiveCra, CraHandle, ORG_ID};
use crate::cra::model::{Consumer, Member, MemberStatus};

type HmacSha256 = Hmac<Sha256>;

pub const HEADER_CONSUMER: &str = "X-MSync-Consumer";
pub const HEADER_KEY_ID: &str = "X-MSync-Key-Id";
pub const HEADER_TIMESTAMP: &str = "X-MSync-Timestamp";
pub const HEADER_NONCE: &str = "X-MSync-Nonce";
pub const HEADER_SIGNATURE: &str = "X-MSync-Signature";

/// The first line of the string to sign and the prefix of the signature header.
pub const SIGNATURE_VERSION: &str = "v1";
/// Largest accepted difference between the request timestamp and the server clock.
pub const MAX_SKEW_SECS: i64 = 300;
/// Nonces are kept this long (twice the timestamp window), then swept.
pub const NONCE_TTL_SECS: i64 = 600;
/// A consumer secret shorter than this is treated as absent.
pub const MIN_SECRET_BYTES: usize = 32;
/// `MSYNC_SUBJECT_KEY` shorter than this is treated as absent.
pub const MIN_SUBJECT_KEY_BYTES: usize = 32;
pub const DEFAULT_RATE_PER_MINUTE: usize = 60;
/// Distinct lookups per consumer per hour above which an alert is raised (hierarchy doc 8.6).
pub const DEFAULT_ALERT_LOOKUPS_PER_HOUR: i64 = 500;
pub const CACHE_TTL_SECONDS: i64 = 60;
/// Prefix of a pairwise subject id.
pub const SUBJECT_PREFIX: &str = "sub_";

/// Where consumer secrets come from: an environment variable name in, its value out.
pub type SecretLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Everything the portal endpoints need. Shared by every worker (`web::Data`).
pub struct PortalContext {
    pub pool: SqlitePool,
    pub cra: CraHandle,
    secrets: SecretLookup,
    subject_key: Option<Vec<u8>>,
    pub rate_per_minute: usize,
    pub alert_lookups_per_hour: i64,
    /// Fix the standing date (tests). Default: today in UTC.
    pub today: Option<NaiveDate>,
    limiter: Mutex<HashMap<String, VecDeque<i64>>>,
    alerted: Mutex<HashMap<String, i64>>,
}

impl PortalContext {
    pub fn new(pool: SqlitePool, cra: CraHandle, secrets: SecretLookup, subject_key: Option<String>) -> Self {
        let subject_key = subject_key.map(|k| k.into_bytes()).filter(|k| k.len() >= MIN_SUBJECT_KEY_BYTES);
        PortalContext {
            pool,
            cra,
            secrets,
            subject_key,
            rate_per_minute: DEFAULT_RATE_PER_MINUTE,
            alert_lookups_per_hour: DEFAULT_ALERT_LOOKUPS_PER_HOUR,
            today: None,
            limiter: Mutex::new(HashMap::new()),
            alerted: Mutex::new(HashMap::new()),
        }
    }

    /// Secrets and keys from the environment: `MSYNC_CONSUMER_SECRET_<KEY>_<KEYID>`,
    /// `MSYNC_SUBJECT_KEY`, `MSYNC_PORTAL_RATE_PER_MINUTE`, `MSYNC_PORTAL_ALERT_LOOKUPS_PER_HOUR`.
    pub fn from_env(pool: SqlitePool, cra: CraHandle) -> Self {
        let raw_key = crate::config::env_var("MSYNC_SUBJECT_KEY");
        match raw_key.as_deref().map(str::len) {
            None => warn!("MSYNC_SUBJECT_KEY is unset: the portal sync API answers 503 to every authenticated lookup"),
            Some(n) if n < MIN_SUBJECT_KEY_BYTES => {
                warn!("MSYNC_SUBJECT_KEY is shorter than {MIN_SUBJECT_KEY_BYTES} bytes and is ignored: the portal sync API answers 503")
            }
            Some(_) => {}
        }
        let mut ctx = Self::new(pool, cra, Arc::new(|name: &str| crate::config::env_var(name)), raw_key);
        if let Some(n) = crate::config::env_var("MSYNC_PORTAL_RATE_PER_MINUTE").and_then(|v| v.trim().parse().ok()) {
            ctx.rate_per_minute = n;
        }
        if let Some(n) = crate::config::env_var("MSYNC_PORTAL_ALERT_LOOKUPS_PER_HOUR").and_then(|v| v.trim().parse().ok()) {
            ctx.alert_lookups_per_hour = n;
        }
        ctx
    }

    pub fn subject_key_configured(&self) -> bool {
        self.subject_key.is_some()
    }

    fn today(&self) -> NaiveDate {
        self.today.unwrap_or_else(|| Utc::now().date_naive())
    }

    /// Sliding one-minute window per consumer. True when the request is within the limit.
    fn rate_ok(&self, consumer: &str, now: i64) -> bool {
        let Ok(mut m) = self.limiter.lock() else { return false };
        let q = m.entry(consumer.to_string()).or_default();
        while q.front().is_some_and(|t| *t <= now - 60) {
            q.pop_front();
        }
        let limit = if audit::mutant("portal_no_rate_limit") { usize::MAX } else { self.rate_per_minute };
        if q.len() >= limit {
            return false;
        }
        q.push_back(now);
        true
    }
}

// ------------------------------------------------------------------ signing

/// The environment variable holding a consumer's secret for one key id: the consumer key and
/// key id uppercased, `-` replaced by `_`. `voltec-portal` key `k1` is
/// `MSYNC_CONSUMER_SECRET_VOLTEC_PORTAL_K1`.
pub fn secret_env_name(consumer: &str, key_id: &str) -> String {
    let up = |s: &str| s.to_ascii_uppercase().replace('-', "_");
    format!("MSYNC_CONSUMER_SECRET_{}_{}", up(consumer), up(key_id))
}

/// The exact string a consumer signs (newline separated, no trailing newline):
///
/// ```text
/// v1
/// GET
/// /api/v1/members/by-discord/123?scopes=community.*
/// 1794470400
/// <nonce>
/// <hex SHA-256 of the body; the empty-string hash for GET>
/// ```
///
/// The path and query are signed exactly as sent, without decoding or reordering.
pub fn string_to_sign(method: &str, path_and_query: &str, timestamp: &str, nonce: &str, body: &[u8]) -> String {
    if audit::mutant("portal_sts_no_version") {
        return format!("{}\n{}\n{}\n{}\n{}", method.to_ascii_uppercase(), path_and_query, timestamp, nonce, hex::encode(Sha256::digest(body)));
    }
    format!(
        "{SIGNATURE_VERSION}\n{}\n{}\n{}\n{}\n{}",
        method.to_ascii_uppercase(),
        path_and_query,
        timestamp,
        nonce,
        hex::encode(Sha256::digest(body))
    )
}

/// The signature header value: `v1=<lowercase hex HMAC-SHA256(secret, string_to_sign)>`.
pub fn sign(secret: &[u8], string_to_sign: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(string_to_sign.as_bytes());
    format!("{SIGNATURE_VERSION}={}", hex::encode(mac.finalize().into_bytes()))
}

/// The pairwise subject id a consumer sees for a member: `sub_` plus the first 32 hex
/// characters of `HMAC-SHA256(consumer_key, member_id)`, where the per-consumer key is
/// `HMAC-SHA256(MSYNC_SUBJECT_KEY, "msync-subject-v1:" + org + ":" + consumer)`. Two consumers
/// get unrelated ids for the same member; one consumer always gets the same id.
pub fn subject_for(subject_key: &[u8], org: &str, consumer: &str, member_id: &str) -> String {
    if audit::mutant("portal_subject_not_pairwise") {
        return format!("{SUBJECT_PREFIX}{member_id}");
    }
    let mut k = HmacSha256::new_from_slice(subject_key).expect("HMAC accepts any key length");
    k.update(format!("msync-subject-v1:{org}:{consumer}").as_bytes());
    let consumer_key = k.finalize().into_bytes();
    let mut m = HmacSha256::new_from_slice(&consumer_key).expect("HMAC accepts any key length");
    m.update(member_id.as_bytes());
    let h = hex::encode(m.finalize().into_bytes());
    format!("{SUBJECT_PREFIX}{}", &h[..32])
}

// ------------------------------------------------------------------ scopes

/// Does scope pattern `inner` grant no more than `outer`? (`community.post` within
/// `community.*`; a pattern within itself.)
fn scope_within(inner: &str, outer: &str) -> bool {
    if inner == outer {
        return true;
    }
    match (inner.strip_suffix(".*"), outer.strip_suffix(".*")) {
        (None, Some(surface)) => inner.split_once('.').is_some_and(|(s, _)| s == surface),
        _ => false,
    }
}

/// Requested scopes intersected with the consumer's allowlist. Asking for more than the
/// allowlist is not an error; it returns nothing more.
pub fn effective_scopes(requested: &[String], allowlist: &[String]) -> Vec<String> {
    let mut out = BTreeSet::new();
    for r in requested {
        for a in allowlist {
            if audit::mutant("portal_scopes_unfiltered") || scope_within(r, a) {
                out.insert(r.clone());
            } else if scope_within(a, r) {
                out.insert(a.clone());
            }
        }
    }
    out.into_iter().collect()
}

/// Is permission `perm` inside one of `scopes`?
pub fn permission_in_scopes(perm: &str, scopes: &[String]) -> bool {
    scopes.iter().any(|s| scope_within(perm, s))
}

fn parse_scopes(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= 64)
        .take(32)
        .map(str::to_string)
        .collect()
}

// ------------------------------------------------------------------ authentication

fn unauthorized() -> HttpResponse {
    HttpResponse::Unauthorized().json(json!({"error": "unauthorized"}))
}

fn not_found() -> HttpResponse {
    HttpResponse::NotFound().json(json!({"error": "not_found"}))
}

fn unavailable() -> HttpResponse {
    HttpResponse::ServiceUnavailable().json(json!({"error": "unavailable"}))
}

fn header<'a>(req: &'a HttpRequest, name: &str) -> Option<&'a str> {
    req.headers().get(name).and_then(|v| v.to_str().ok()).map(str::trim)
}

fn valid_key_id(k: &str) -> bool {
    (1..=16).contains(&k.len()) && k.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// 22 to 128 base64url characters (128 random bits encode to 22).
fn valid_nonce(n: &str) -> bool {
    (22..=128).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Why a request was refused. Never shown to the caller: every variant is the same 401.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    MissingHeader,
    UnknownConsumer,
    UnknownKey,
    StaleTimestamp,
    BadSignature,
    ReplayedNonce,
    RateLimited,
}

pub struct Authenticated<'a> {
    pub consumer: &'a Consumer,
    pub key_id: String,
}

/// The five checks of hierarchy doc 8.2, in order.
pub async fn authenticate<'a>(
    ctx: &PortalContext,
    active: &'a ActiveCra,
    req: &HttpRequest,
    body: &[u8],
    now: i64,
) -> Result<Authenticated<'a>, Refusal> {
    let (Some(consumer), Some(key_id), Some(ts), Some(nonce), Some(sig)) = (
        header(req, HEADER_CONSUMER),
        header(req, HEADER_KEY_ID),
        header(req, HEADER_TIMESTAMP),
        header(req, HEADER_NONCE),
        header(req, HEADER_SIGNATURE),
    ) else {
        return Err(Refusal::MissingHeader);
    };
    let path_and_query = if audit::mutant("portal_sts_drop_query") {
        req.uri().path()
    } else {
        req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/")
    };
    let sts = string_to_sign(req.method().as_str(), path_and_query, ts, nonce, body);

    // 1. The consumer is in the active CRA and this key id has a usable secret. An unknown
    //    consumer or key still costs one HMAC, so timing does not tell them apart.
    let mut found = active.snapshot.org.consumers.iter().find(|c| c.key == consumer);
    if found.is_none() && audit::mutant("portal_unknown_consumer_uses_first") {
        found = active.snapshot.org.consumers.first();
    }
    let key_for_lookup = if audit::mutant("portal_any_key_id") { "k1" } else { key_id };
    let secret = match found {
        Some(c) if valid_key_id(key_for_lookup) => {
            (ctx.secrets)(&secret_env_name(&c.key, key_for_lookup)).filter(|s| s.len() >= MIN_SECRET_BYTES)
        }
        _ => None,
    };
    let Some(c) = found else {
        let _ = sign(b"msync-unknown-consumer-padding-key", &sts);
        return Err(Refusal::UnknownConsumer);
    };
    let Some(secret) = secret else {
        let _ = sign(b"msync-unknown-consumer-padding-key", &sts);
        return Err(Refusal::UnknownKey);
    };

    // 2. Timestamp within the window.
    let Ok(t) = ts.parse::<i64>() else { return Err(Refusal::StaleTimestamp) };
    if !audit::mutant("portal_no_timestamp_check") && (now - t).abs() > MAX_SKEW_SECS {
        return Err(Refusal::StaleTimestamp);
    }

    // 3. Signature, constant time.
    let presented = sig.strip_prefix("v1=").and_then(|h| hex::decode(h).ok());
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(sts.as_bytes());
    let good = match presented {
        Some(p) => mac.verify_slice(&p).is_ok(),
        None => false,
    };
    if !good && !audit::mutant("portal_no_signature_check") {
        return Err(Refusal::BadSignature);
    }

    // 4. Nonce is new. Expired nonces are swept first.
    if !valid_nonce(nonce) {
        return Err(Refusal::ReplayedNonce);
    }
    let fresh = async {
        sqlx::query("DELETE FROM consumer_nonces WHERE seen_at < ?").bind(now - NONCE_TTL_SECS).execute(&ctx.pool).await?;
        if audit::mutant("portal_no_nonce_check") {
            return Ok::<bool, sqlx::Error>(true);
        }
        let res = sqlx::query("INSERT OR IGNORE INTO consumer_nonces (consumer, nonce, seen_at) VALUES (?, ?, ?)")
            .bind(&c.key)
            .bind(nonce)
            .bind(now)
            .execute(&ctx.pool)
            .await?;
        Ok(res.rows_affected() == 1)
    }
    .await;
    match fresh {
        Ok(true) => {}
        Ok(false) => return Err(Refusal::ReplayedNonce),
        Err(e) => {
            // Fail closed: a nonce that cannot be recorded cannot be proven new.
            error!("portal nonce store error: {e}");
            return Err(Refusal::ReplayedNonce);
        }
    }

    // 5. Per-consumer rate limit.
    if !ctx.rate_ok(&c.key, now) {
        return Err(Refusal::RateLimited);
    }
    Ok(Authenticated { consumer: c, key_id: key_id.to_string() })
}

// ------------------------------------------------------------------ lookups

/// What a request looked up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    Discord(String),
    Slack { team: String, user: String },
    Subject(String),
}

impl Lookup {
    fn label(&self) -> String {
        match self {
            Lookup::Discord(id) => format!("discord:{id}"),
            Lookup::Slack { team, user } => format!("slack:{team}:{user}"),
            Lookup::Subject(s) => format!("subject:{s}"),
        }
    }
}

/// The member a lookup names, if this consumer may see them.
fn find_member<'a>(active: &'a ActiveCra, subject_key: &[u8], consumer: &str, lookup: &Lookup) -> Option<&'a Member> {
    let snap = &active.snapshot;
    match lookup {
        Lookup::Discord(id) => snap.members.members.iter().find(|m| m.identities.discord.as_deref() == Some(id.as_str())),
        Lookup::Slack { team, user } => {
            // The Slack identity is valid only in the team the CRA names.
            let sl = snap.org.slack.as_ref()?;
            if &sl.team_id != team {
                return None;
            }
            snap.members.members.iter().find(|m| m.identities.slack.as_deref() == Some(user.as_str()))
        }
        Lookup::Subject(s) => {
            if !s.starts_with(SUBJECT_PREFIX) || s.len() != SUBJECT_PREFIX.len() + 32 {
                return None;
            }
            snap.members.members.iter().find(|m| {
                let mine = subject_for(subject_key, ORG_ID, consumer, &m.id);
                bool::from(subtle::ConstantTimeEq::ct_eq(mine.as_bytes(), s.as_bytes()))
            })
        }
    }
}

/// The response body for a found member, limited to the consumer's field allowlist and
/// effective scopes (hierarchy doc 8.4).
#[allow(clippy::too_many_arguments)]
pub fn member_view(
    active: &ActiveCra,
    consumer: &Consumer,
    member: &Member,
    subject: &str,
    merit: u64,
    granted: &[String],
    today: NaiveDate,
    issued_at: &str,
) -> Value {
    let fields: BTreeSet<&str> = consumer.fields.iter().map(String::as_str).collect();
    let allowed = |f: &str| audit::mutant("portal_fields_unfiltered") || fields.contains(f);
    let st = crate::cra::standing(&active.snapshot, &member.id, merit, today);
    let status = member.status;
    let mut o = Map::new();
    o.insert("subject".into(), json!(subject));
    // Always present: a consumer must learn a suspension to revoke what it granted.
    o.insert("status".into(), json!(status.as_str()));
    if status == MemberStatus::Active {
        if let Some(st) = &st {
            if allowed("tier") {
                if let Some(t) = &st.tier {
                    o.insert("tier".into(), json!({"key": t.key, "label": t.title, "rank": t.rank}));
                }
            }
            let pos_fields: Vec<&str> =
                ["positions.title", "positions.rank", "positions.expires_at"].into_iter().filter(|f| allowed(f)).collect();
            if !pos_fields.is_empty() {
                let list: Vec<Value> = st
                    .positions
                    .iter()
                    .map(|p| {
                        let mut x = Map::new();
                        if pos_fields.contains(&"positions.title") {
                            x.insert("title".into(), json!(p.title));
                        }
                        if pos_fields.contains(&"positions.rank") {
                            x.insert("rank".into(), json!(p.rank));
                        }
                        if pos_fields.contains(&"positions.expires_at") {
                            x.insert("expires_at".into(), json!(p.expires.map(|d| d.format("%Y-%m-%d").to_string())));
                        }
                        Value::Object(x)
                    })
                    .collect();
                o.insert("positions".into(), Value::Array(list));
            }
            if allowed("merit.vested") {
                o.insert("merit".into(), json!({"vested": merit}));
            }
        }
    }
    if allowed("permissions") {
        let perms: Vec<String> = match (&st, status, audit::mutant("portal_suspended_keeps_permissions")) {
            (Some(st), MemberStatus::Active, _) | (Some(st), _, true) => {
                st.permissions
                    .iter()
                    .filter(|p| permission_in_scopes(p, granted) || audit::mutant("portal_permissions_unfiltered"))
                    .cloned()
                    .collect()
            }
            _ => Vec::new(),
        };
        o.insert("permissions".into(), json!(perms));
    }
    o.insert("scopes_granted".into(), json!(granted));
    o.insert("policy_version".into(), json!(active.snapshot_id));
    o.insert("issued_at".into(), json!(issued_at));
    o.insert("cache_ttl_seconds".into(), json!(CACHE_TTL_SECONDS));
    Value::Object(o)
}

#[derive(Debug, Default, Deserialize)]
struct ScopesQuery {
    #[serde(default)]
    scopes: Option<String>,
}

/// Record and audit one authenticated lookup. Fails when either write fails.
#[allow(clippy::too_many_arguments)]
async fn record_lookup(
    ctx: &PortalContext,
    auth: &Authenticated<'_>,
    lookup: &Lookup,
    member: Option<&Member>,
    subject: Option<&str>,
    requested: &[String],
    granted: &[String],
    now: i64,
) -> Result<(), sqlx::Error> {
    let result = if member.is_some() { "found" } else { "not_found" };
    let refused: Vec<&String> = requested.iter().filter(|r| !granted.contains(r)).collect();
    if !refused.is_empty() {
        warn!("portal consumer {} asked for scopes outside its allowlist: {:?}", auth.consumer.key, refused);
    }
    let mut tx = ctx.pool.begin().await?;
    sqlx::query(
        "INSERT INTO consumer_lookups (org_id, consumer, key_id, lookup, subject, result, scopes_requested, scopes_granted, at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(ORG_ID)
    .bind(&auth.consumer.key)
    .bind(&auth.key_id)
    .bind(lookup.label())
    .bind(subject)
    .bind(result)
    .bind(serde_json::to_string(requested).unwrap_or_default())
    .bind(serde_json::to_string(granted).unwrap_or_default())
    .bind(now)
    .execute(&mut *tx)
    .await?;
    audit::append_in(
        &mut tx,
        &AuditEntry::new(
            ActorKind::Consumer,
            format!("consumer:{}", auth.consumer.key),
            "portal.lookup",
            subject.map(|s| format!("subject:{s}")).unwrap_or_else(|| lookup.label()),
            json!({
                "consumer": auth.consumer.key,
                "key_id": auth.key_id,
                "lookup": lookup.label(),
                "result": result,
                "member_id": member.map(|m| m.id.clone()),
                "status": member.map(|m| m.status.as_str()),
                "subject": subject,
                "scopes_requested": requested,
                "scopes_granted": granted,
                "scopes_refused": refused,
            }),
        ),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Raise the lookup-rate alert once an hour per consumer when its distinct lookups in the last
/// hour exceed the threshold.
async fn check_alert(ctx: &PortalContext, consumer: &str, now: i64) {
    let n: Result<i64, _> = sqlx::query_scalar("SELECT COUNT(DISTINCT lookup) FROM consumer_lookups WHERE consumer = ? AND at > ?")
        .bind(consumer)
        .bind(now - 3600)
        .fetch_one(&ctx.pool)
        .await;
    let Ok(n) = n else { return };
    if n <= ctx.alert_lookups_per_hour {
        return;
    }
    let first = match ctx.alerted.lock() {
        Ok(mut m) => {
            let due = m.get(consumer).is_none_or(|t| *t <= now - 3600);
            if due {
                m.insert(consumer.to_string(), now);
            }
            due
        }
        Err(_) => true,
    };
    if !first {
        return;
    }
    error!(
        "PORTAL ALERT: consumer {consumer} made {n} distinct lookups in the last hour (threshold {}); a stolen consumer secret looks like this",
        ctx.alert_lookups_per_hour
    );
    audit::append_best_effort(
        &ctx.pool,
        &AuditEntry::new(
            ActorKind::System,
            "system:portal",
            "portal.lookup_alert",
            format!("consumer:{consumer}"),
            json!({"consumer": consumer, "distinct_lookups_last_hour": n, "threshold": ctx.alert_lookups_per_hour}),
        ),
    )
    .await;
}

/// One portal request, start to finish.
pub async fn handle(ctx: &PortalContext, req: &HttpRequest, lookup: Lookup) -> HttpResponse {
    let now = Utc::now().timestamp();
    // No snapshot: no consumers and no members. The same answer as an unknown member.
    let Some(active) = ctx.cra.active() else { return not_found() };
    let auth = match authenticate(ctx, &active, req, b"", now).await {
        Ok(a) => a,
        Err(why) => {
            warn!("portal request refused ({why:?}) for consumer {:?}", header(req, HEADER_CONSUMER));
            return unauthorized();
        }
    };
    let Some(subject_key) = ctx.subject_key.as_deref() else {
        error!("portal lookup by {} refused: MSYNC_SUBJECT_KEY is not configured", auth.consumer.key);
        return unavailable();
    };
    let q = web::Query::<ScopesQuery>::from_query(req.query_string()).map(|q| q.into_inner()).unwrap_or_default();
    let requested = parse_scopes(q.scopes.as_deref());
    let granted = effective_scopes(&requested, &auth.consumer.scopes);
    let member = find_member(&active, subject_key, &auth.consumer.key, &lookup);
    let subject = member.map(|m| subject_for(subject_key, ORG_ID, &auth.consumer.key, &m.id));
    if let Err(e) = record_lookup(ctx, &auth, &lookup, member, subject.as_deref(), &requested, &granted, now).await {
        error!("portal lookup by {} could not be recorded; refusing to answer: {e}", auth.consumer.key);
        return unavailable();
    }
    check_alert(ctx, &auth.consumer.key, now).await;
    if member.is_none() && matches!(lookup, Lookup::Subject(_)) && audit::mutant("portal_hidden_403") {
        return HttpResponse::Forbidden().json(json!({"error": "forbidden"}));
    }
    let (Some(member), Some(subject)) = (member, subject) else { return not_found() };
    let issued_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let merit = ctx.cra.merit_of(&member.id);
    HttpResponse::Ok().json(member_view(&active, auth.consumer, member, &subject, merit, &granted, ctx.today(), &issued_at))
}

async fn by_discord(req: HttpRequest, path: web::Path<String>, ctx: web::Data<PortalContext>) -> HttpResponse {
    handle(&ctx, &req, Lookup::Discord(path.into_inner())).await
}

async fn by_slack(req: HttpRequest, path: web::Path<(String, String)>, ctx: web::Data<PortalContext>) -> HttpResponse {
    let (team, user) = path.into_inner();
    handle(&ctx, &req, Lookup::Slack { team, user }).await
}

async fn by_subject(req: HttpRequest, path: web::Path<String>, ctx: web::Data<PortalContext>) -> HttpResponse {
    handle(&ctx, &req, Lookup::Subject(path.into_inner())).await
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(web::resource("/api/v1/members/by-discord/{discord_id}").route(web::get().to(by_discord)))
        .service(web::resource("/api/v1/members/by-slack/{team_id}/{user_id}").route(web::get().to(by_slack)))
        .service(web::resource("/api/v1/members/{subject}").route(web::get().to(by_subject)));
}
