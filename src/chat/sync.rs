//! Role sync and drift: the chat platforms as projections of the CRA (contract 13.3,
//! hierarchy doc 7.3).
//!
//! A platform implements one small trait, [`SyncPlatform`]: read the current holders of the
//! managed roles, add a member to a role, remove a member from a role, post an alert, and
//! say whether a role is safe to manage. Discord implements it over serenity HTTP
//! (`crate::discord::sync`), Slack over its Web API with user groups (`crate::slack::sync`).
//! Everything else here is written once, against the trait.
//!
//! One reconcile run:
//!
//! 1. The desired state is the active snapshot's projection for the platform
//!    ([`crate::chat::commands::projection_plan`]: `discord_projection` or `slack_projection`
//!    with the cached merit).
//! 2. The managed roles are exactly the roles the CRA maps. Each is checked with
//!    [`SyncPlatform::role_safety`] first; a role that is dangerous (a Discord role with
//!    Administrator, Manage Roles, Manage Server, Ban Members, Kick Members or Manage
//!    Webhooks), at or above the bot's own highest role, or missing is not managed at all:
//!    it is neither read nor written, and it is reported.
//! 3. The holders of the safe managed roles are read. Roles the CRA does not map are never
//!    asked for and never changed.
//! 4. Every difference is applied. A difference is **drift** when the pair was already as
//!    the projection wants it at the end of the previous run and the projection has not
//!    changed since (someone changed it by hand), or when the holder is not a CRA member on
//!    this platform at all (the CRA can never have put them there). Drift is reverted,
//!    recorded in `chat_drift_events` and reported to the platform's alert channel.
//! 5. Anti-flap: after [`FLAP_LIMIT`] reverts of the same (platform, role, member) within
//!    [`FLAP_WINDOW_SECS`], the pair is held: no more reverts, one alert, and a row in
//!    `chat_drift_holds`. A hold lasts until the next snapshot activation or an explicit
//!    `sync` by an authorized member.
//!
//! Failure handling: a change counts as applied only after the platform said yes; a failed
//! change is retried by the next run; a rate limit stops the run and backs off until the
//! platform's retry time; a platform that is not configured is skipped without error.
//!
//! Runs for one platform never overlap: each [`SyncRunner`] holds one lock for the whole
//! run, and the background task ([`spawn`]) feeds every trigger (snapshot activation,
//! member join, member update, the timer) through one queue.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use super::{ChatPlatform, ProjectionPlan, SyncReport};
use crate::cra::loader::{cached_merits, CraHandle};

/// `MSYNC_ROLE_SYNC_SECS` default: a full reconcile every five minutes.
pub const DEFAULT_ROLE_SYNC_SECS: u64 = 300;
/// Reverts of one (platform, role, member) allowed inside [`FLAP_WINDOW_SECS`] before the
/// pair is held.
pub const FLAP_LIMIT: i64 = 3;
/// The anti-flap window (one hour).
pub const FLAP_WINDOW_SECS: i64 = 3600;
/// Back-off after a rate limit that named no retry time.
pub const DEFAULT_BACKOFF_SECS: u64 = 60;

/// Test-only mutation switch: `MSYNC_SYNC_MUTANT=<name>` turns one guard off so a test can
/// be shown to fail. Always false outside `cargo test`.
#[inline]
pub(crate) fn mutant(name: &str) -> bool {
    cfg!(test) && std::env::var("MSYNC_SYNC_MUTANT").is_ok_and(|v| v == name)
}

// ------------------------------------------------------------------ the trait

/// What a platform call can fail with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformError {
    /// HTTP 429 or the platform's rate-limit error. The run stops and backs off.
    RateLimited { retry_after_secs: Option<u64> },
    /// The platform will not do this (for example, Slack refuses an empty user group).
    Refused(String),
    /// Anything else: network, permissions, unknown ids.
    Api(String),
}

impl std::fmt::Display for PlatformError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlatformError::RateLimited { retry_after_secs: Some(s) } => write!(f, "rate limited (retry after {s}s)"),
            PlatformError::RateLimited { retry_after_secs: None } => write!(f, "rate limited"),
            PlatformError::Refused(m) => write!(f, "refused: {m}"),
            PlatformError::Api(m) => write!(f, "{m}"),
        }
    }
}

/// Whether MSync may manage a mapped role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleSafety {
    Safe,
    /// The role carries an administrator-class permission (the names are listed). Merit and
    /// job titles never confer authority on the platform.
    Dangerous(Vec<String>),
    /// The role is at or above the bot's own highest role; the platform would refuse anyway.
    AboveBot,
    /// The platform does not have this role.
    Missing,
}

impl RoleSafety {
    pub fn is_safe(&self) -> bool {
        matches!(self, RoleSafety::Safe)
    }

    /// Why the role is not managed, in words.
    pub fn reason(&self) -> String {
        match self {
            RoleSafety::Safe => "safe".into(),
            RoleSafety::Dangerous(p) => format!("it carries {}", p.join(", ")),
            RoleSafety::AboveBot => "it is at or above the bot's highest role; move the MSync bot role above every managed role".into(),
            RoleSafety::Missing => "the platform has no such role".into(),
        }
    }
}

/// One chat platform as role sync sees it.
#[async_trait]
pub trait SyncPlatform: Send + Sync {
    fn platform(&self) -> ChatPlatform;

    /// Credentials and a workspace are configured. A platform that is not configured is
    /// skipped without error.
    fn configured(&self) -> bool;

    /// Whether each of `roles` may be managed. Called before anything is read.
    async fn role_safety(&self, roles: &[String]) -> Result<BTreeMap<String, RoleSafety>, PlatformError>;

    /// The current holders of each of `roles`, and of nothing else.
    async fn holders(&self, roles: &[String]) -> Result<BTreeMap<String, BTreeSet<String>>, PlatformError>;

    async fn add_member(&self, role: &str, user: &str) -> Result<(), PlatformError>;

    async fn remove_member(&self, role: &str, user: &str) -> Result<(), PlatformError>;

    /// Post to the alert channel the CRA names for this platform.
    async fn alert(&self, text: &str) -> Result<(), PlatformError>;
}

// ------------------------------------------------------------ events (seam)

/// What a hand edit did to a managed role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftDirection {
    /// Someone granted the role by hand; MSync removes it.
    Granted,
    /// Someone removed the role by hand; MSync restores it.
    Removed,
}

impl DriftDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            DriftDirection::Granted => "granted",
            DriftDirection::Removed => "removed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncEventKind {
    /// Drift reverted.
    DriftReverted,
    /// Drift observed and not reverted: the pair is held (anti-flap).
    DriftHeld,
    /// A projection change applied (not drift).
    ChangeApplied,
    /// A mapped role refused as unsafe.
    RoleRefused,
    /// Holds released (snapshot activation or an explicit `sync`).
    HoldsReleased,
}

/// One role sync event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncEvent {
    pub platform: ChatPlatform,
    pub kind: SyncEventKind,
    pub role: String,
    pub member: String,
    pub direction: Option<DriftDirection>,
    pub observed_at: DateTime<Utc>,
    pub reverted_at: Option<DateTime<Utc>>,
    pub snapshot_id: Option<i64>,
    pub detail: String,
}

/// The audit action for a role sync event.
pub fn sync_audit_action(kind: SyncEventKind) -> &'static str {
    match kind {
        SyncEventKind::DriftReverted => "chat.sync.drift_reverted",
        SyncEventKind::DriftHeld => "chat.sync.drift_held",
        SyncEventKind::ChangeApplied => "chat.sync.change_applied",
        SyncEventKind::RoleRefused => "chat.sync.role_refused",
        SyncEventKind::HoldsReleased => "chat.sync.holds_released",
    }
}

/// The seam for the audit log (contract 13.1: "role sync changes and drift"). Every event
/// is appended to the audit log, best effort (a failed audit write is logged and never
/// stops role sync). Drift (reverted and held) is also written to `chat_drift_events`, which
/// the anti-flap hold reads. Returns the drift row id when one was written.
pub async fn record_sync_event(pool: &SqlitePool, ev: &SyncEvent) -> Result<Option<i64>, sqlx::Error> {
    let mut entry = crate::audit::AuditEntry::new(
        crate::audit::ActorKind::System,
        "system:role-sync",
        sync_audit_action(ev.kind),
        format!("{}:{}:{}", ev.platform.as_str(), ev.role, ev.member),
        serde_json::json!({
            "platform": ev.platform.as_str(),
            "role": ev.role,
            "member": ev.member,
            "direction": ev.direction.map(|d| d.as_str()),
            "observed_at": ts(ev.observed_at),
            "reverted_at": ev.reverted_at.map(ts),
            "snapshot_id": ev.snapshot_id,
            "detail": ev.detail,
        }),
    );
    entry.org_id = crate::cra::loader::ORG_ID.to_string();
    crate::audit::append_best_effort(pool, &entry).await;
    let outcome = match ev.kind {
        SyncEventKind::DriftReverted => "reverted",
        SyncEventKind::DriftHeld => "held",
        _ if mutant("record_everything") => "reverted",
        other => {
            info!(
                "role sync {:?} on {}: role {} member {} {}",
                other,
                ev.platform.label(),
                ev.role,
                ev.member,
                ev.detail
            );
            return Ok(None);
        }
    };
    let res = sqlx::query(
        "INSERT INTO chat_drift_events (platform, role_id, member_id, direction, outcome, observed_at, reverted_at, snapshot_id, detail)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(ev.platform.as_str())
    .bind(&ev.role)
    .bind(&ev.member)
    .bind(ev.direction.map(|d| d.as_str()).unwrap_or("granted"))
    .bind(outcome)
    .bind(ts(ev.observed_at))
    .bind(ev.reverted_at.map(ts))
    .bind(ev.snapshot_id)
    .bind(&ev.detail)
    .execute(pool)
    .await?;
    Ok(Some(res.last_insert_rowid()))
}

/// Timestamps are stored as RFC 3339 UTC with milliseconds, so they compare as strings.
pub fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

type Pair = (String, String); // (role, user)

async fn open_holds(pool: &SqlitePool, p: ChatPlatform) -> Result<BTreeSet<Pair>, sqlx::Error> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT role_id, member_id FROM chat_drift_holds WHERE platform = ? AND released_at IS NULL")
            .bind(p.as_str())
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().collect())
}

/// Reverts of one pair inside the anti-flap window, counted from the later of one hour ago
/// and the pair's last release.
async fn recent_reverts(pool: &SqlitePool, p: ChatPlatform, role: &str, user: &str, now: DateTime<Utc>) -> Result<i64, sqlx::Error> {
    let last_release: Option<String> = sqlx::query_scalar(
        "SELECT MAX(released_at) FROM chat_drift_holds WHERE platform = ? AND role_id = ? AND member_id = ?",
    )
    .bind(p.as_str())
    .bind(role)
    .bind(user)
    .fetch_one(pool)
    .await?;
    let window = ts(now - chrono::Duration::seconds(FLAP_WINDOW_SECS));
    let since = match last_release {
        Some(r) if r > window => r,
        _ => window,
    };
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM chat_drift_events
         WHERE platform = ? AND role_id = ? AND member_id = ? AND outcome = 'reverted' AND reverted_at >= ?",
    )
    .bind(p.as_str())
    .bind(role)
    .bind(user)
    .bind(since)
    .fetch_one(pool)
    .await
}

async fn open_hold(pool: &SqlitePool, p: ChatPlatform, role: &str, user: &str, now: DateTime<Utc>) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT OR IGNORE INTO chat_drift_holds (platform, role_id, member_id, held_at) VALUES (?, ?, ?, ?)")
        .bind(p.as_str())
        .bind(role)
        .bind(user)
        .bind(ts(now))
        .execute(pool)
        .await?;
    Ok(())
}

/// Release every open hold on `p`. Returns how many were released.
pub async fn release_holds(pool: &SqlitePool, p: ChatPlatform, reason: &str, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE chat_drift_holds SET released_at = ?, release_reason = ? WHERE platform = ? AND released_at IS NULL",
    )
    .bind(ts(now))
    .bind(reason)
    .bind(p.as_str())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

// ------------------------------------------------------------------- reports

/// Why a run started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Trigger {
    Timer,
    MemberUpdate,
    MemberJoin,
    /// An explicit `sync` by an authorized member: releases holds.
    Command,
    /// A snapshot was activated: releases holds.
    Activation,
}

impl Trigger {
    fn releases_holds(self) -> bool {
        !mutant("no_release") && matches!(self, Trigger::Command | Trigger::Activation)
    }
}

/// One drift item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drift {
    pub role: String,
    pub user: String,
    pub direction: DriftDirection,
}

/// What one run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Set when the run did nothing on purpose (not configured, no CRA, backing off).
    pub skipped: Option<String>,
    /// Projection changes applied (not drift).
    pub added: Vec<Pair>,
    pub removed: Vec<Pair>,
    /// Drift reverted this run.
    pub reverted: Vec<Drift>,
    /// Pairs held this run (the anti-flap limit was reached now).
    pub newly_held: Vec<Drift>,
    /// Pairs still held from earlier runs, left as they are.
    pub held: Vec<Pair>,
    /// Mapped roles not managed, with the reason.
    pub refused: Vec<(String, RoleSafety)>,
    /// Changes the platform did not accept; retried next run.
    pub failed: Vec<(String, String, String)>,
    pub rate_limited: bool,
    /// The run could not read the platform or the database; nothing was applied.
    pub error: Option<String>,
    pub holds_released: u64,
}

impl ReconcileReport {
    fn skipped(why: impl Into<String>) -> Self {
        ReconcileReport { skipped: Some(why.into()), ..Default::default() }
    }

    pub fn applied(&self) -> usize {
        self.added.len() + self.removed.len() + self.reverted.len()
    }

    /// The reply the `sync` command shows.
    pub fn to_sync_report(&self, p: ChatPlatform) -> SyncReport {
        let mut notes = Vec::new();
        let noun = role_noun(p);
        if let Some(s) = &self.skipped {
            notes.push(format!("Skipped: {s}"));
        }
        if let Some(e) = &self.error {
            notes.push(format!("❌ Nothing applied: {e}"));
        }
        if self.holds_released > 0 {
            notes.push(format!("Released {} held drift pair(s).", self.holds_released));
        }
        if !self.added.is_empty() || !self.removed.is_empty() {
            notes.push(format!("Projection changes: {} added, {} removed.", self.added.len(), self.removed.len()));
        }
        for d in &self.reverted {
            notes.push(format!("↩️ Drift reverted: {noun} `{}` was {} by hand for `{}`.", d.role, d.direction.as_str(), d.user));
        }
        for d in &self.newly_held {
            notes.push(format!("⏸️ Held: {noun} `{}` for `{}` changed by hand {FLAP_LIMIT} times within an hour; no longer reverted.", d.role, d.user));
        }
        for (r, u) in &self.held {
            notes.push(format!("⏸️ Still held: {noun} `{r}` for `{u}`."));
        }
        for (r, why) in &self.refused {
            notes.push(format!("🚫 Not managed: {noun} `{r}`: {}.", why.reason()));
        }
        for (r, u, e) in &self.failed {
            notes.push(format!("✗ `{r}` for `{u}` failed: {e}"));
        }
        if self.rate_limited {
            notes.push("⏳ Rate limited: the rest is retried on the next run.".into());
        }
        SyncReport { synced: self.applied() as u32, failed: self.failed.len() as u32, notes }
    }

    /// The alert-channel text, or `None` when nothing needs reporting.
    fn alert_text(&self, p: ChatPlatform, new_refusals: &[(String, RoleSafety)]) -> Option<String> {
        let noun = role_noun(p);
        let mut lines = Vec::new();
        for d in &self.reverted {
            lines.push(match d.direction {
                DriftDirection::Granted => format!("↩️ {noun} `{}` was granted by hand to `{}` and is not in the CRA projection; removed.", d.role, d.user),
                DriftDirection::Removed => format!("↩️ {noun} `{}` was removed by hand from `{}`, who holds it in the CRA; restored.", d.role, d.user),
            });
        }
        for d in &self.newly_held {
            lines.push(format!(
                "⏸️ {noun} `{}` for `{}` was changed by hand {FLAP_LIMIT} times within an hour. MSync stops reverting it until the next snapshot activation or an explicit sync.",
                d.role, d.user
            ));
        }
        for (r, why) in new_refusals {
            lines.push(format!("🚫 {noun} `{r}` is mapped in the CRA but not managed: {}.", why.reason()));
        }
        if lines.is_empty() {
            None
        } else {
            Some(format!("**MSync role sync ({})**\n{}", p.label(), lines.join("\n")))
        }
    }
}

fn role_noun(p: ChatPlatform) -> &'static str {
    match p {
        ChatPlatform::Discord => "Role",
        ChatPlatform::Slack => "User group",
    }
}

// -------------------------------------------------------------------- runner

pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

#[derive(Default)]
struct RunnerState {
    /// For each (role, user) seen at the end of the last run: did the platform hold it,
    /// matching the projection? Absent: unknown (never seen, failed, or held).
    settled: BTreeMap<Pair, bool>,
    backoff_until: Option<DateTime<Utc>>,
    /// Refusals already alerted, so a refused role is reported once, not every tick.
    alerted_refusals: BTreeSet<String>,
    /// Mutation `stale_plan` only.
    cached_plan: Option<(ProjectionPlan, i64)>,
}

/// Reconciles one platform. One lock per runner: two runs never overlap.
pub struct SyncRunner {
    platform: Arc<dyn SyncPlatform>,
    pool: SqlitePool,
    cra: CraHandle,
    today: Option<NaiveDate>,
    clock: Clock,
    state: tokio::sync::Mutex<RunnerState>,
}

impl SyncRunner {
    pub fn new(platform: Arc<dyn SyncPlatform>, pool: SqlitePool, cra: CraHandle) -> Self {
        SyncRunner {
            platform,
            pool,
            cra,
            today: None,
            clock: Arc::new(Utc::now),
            state: tokio::sync::Mutex::new(RunnerState::default()),
        }
    }

    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_today(mut self, today: NaiveDate) -> Self {
        self.today = Some(today);
        self
    }

    pub fn platform(&self) -> ChatPlatform {
        self.platform.platform()
    }

    pub fn cra(&self) -> &CraHandle {
        &self.cra
    }

    fn today(&self) -> NaiveDate {
        if mutant("today_fixed") {
            return NaiveDate::from_ymd_opt(2026, 11, 12).unwrap();
        }
        self.today.unwrap_or_else(|| (self.clock)().date_naive())
    }

    /// The projection for this platform from the active snapshot, or `None` with no CRA.
    pub fn plan(&self) -> Option<(ProjectionPlan, i64)> {
        let active = self.cra.active()?;
        let plan = super::commands::projection_plan(&active.snapshot, self.platform(), &cached_merits(&self.cra), self.today());
        Some((plan, active.snapshot_id))
    }

    /// One run from the active snapshot.
    pub async fn run(&self, trigger: Trigger) -> ReconcileReport {
        if mutant("no_lock") {
            let mut unlocked = RunnerState::default();
            let plan = self.plan();
            return self.run_locked(&mut unlocked, trigger, plan.as_ref().map(|(p, id)| (p, Some(*id)))).await;
        }
        let mut state = self.state.lock().await;
        let mut plan = self.plan();
        if mutant("stale_plan") {
            if state.cached_plan.is_none() {
                state.cached_plan = plan.clone();
            }
            plan = state.cached_plan.clone();
        }
        self.run_locked(&mut state, trigger, plan.as_ref().map(|(p, id)| (p, Some(*id)))).await
    }

    /// One run with a plan computed by the caller (the `sync` command).
    pub async fn run_with_plan(&self, trigger: Trigger, plan: &ProjectionPlan) -> ReconcileReport {
        let mut state = self.state.lock().await;
        let id = self.cra.active().map(|a| a.snapshot_id);
        self.run_locked(&mut state, trigger, Some((plan, id))).await
    }

    async fn run_locked(&self, state: &mut RunnerState, trigger: Trigger, plan: Option<(&ProjectionPlan, Option<i64>)>) -> ReconcileReport {
        let p = self.platform();
        if !self.platform.configured() {
            return ReconcileReport::skipped(format!("{} is not configured", p.label()));
        }
        let Some((plan, snapshot_id)) = plan else {
            return ReconcileReport::skipped("no CRA snapshot is active");
        };
        let now = (self.clock)();
        if let Some(until) = state.backoff_until {
            if now < until {
                return ReconcileReport::skipped(format!("rate limited; backing off until {}", ts(until)));
            }
            state.backoff_until = None;
        }

        let mut report = ReconcileReport::default();
        if trigger.releases_holds() {
            let reason = if trigger == Trigger::Activation { "snapshot activation" } else { "explicit sync" };
            match release_holds(&self.pool, p, reason, now).await {
                Ok(n) => {
                    report.holds_released = n;
                    if n > 0 {
                        let _ = record_sync_event(
                            &self.pool,
                            &SyncEvent {
                                platform: p,
                                kind: SyncEventKind::HoldsReleased,
                                role: String::new(),
                                member: String::new(),
                                direction: None,
                                observed_at: now,
                                reverted_at: None,
                                snapshot_id,
                                detail: format!("{n} hold(s) released by {reason}"),
                            },
                        )
                        .await;
                    }
                }
                Err(e) => {
                    report.error = Some(format!("database: {e}"));
                    return report;
                }
            }
            if trigger == Trigger::Activation {
                state.alerted_refusals.clear();
            }
        }

        // 1. Which mapped roles may be managed. Nothing is read before this.
        let mut managed: Vec<String> = Vec::new();
        for r in &plan.managed {
            if !managed.contains(r) {
                managed.push(r.clone());
            }
        }
        if managed.is_empty() {
            return report;
        }
        let safety = match self.platform.role_safety(&managed).await {
            Ok(s) => s,
            Err(e) => return self.read_failed(state, report, e, now),
        };
        let mut safe = Vec::new();
        for r in &managed {
            match safety.get(r).cloned().unwrap_or(RoleSafety::Missing) {
                _ if mutant("no_safety") => safe.push(r.clone()),
                RoleSafety::Safe => safe.push(r.clone()),
                other => report.refused.push((r.clone(), other)),
            }
        }
        let new_refusals: Vec<(String, RoleSafety)> =
            report.refused.iter().filter(|(r, _)| !state.alerted_refusals.contains(r)).cloned().collect();
        for (r, why) in &new_refusals {
            let _ = record_sync_event(
                &self.pool,
                &SyncEvent {
                    platform: p,
                    kind: SyncEventKind::RoleRefused,
                    role: r.clone(),
                    member: String::new(),
                    direction: None,
                    observed_at: now,
                    reverted_at: None,
                    snapshot_id,
                    detail: why.reason(),
                },
            )
            .await;
        }
        // A role that became safe may be refused again later and must be reported again.
        state.alerted_refusals = report.refused.iter().map(|(r, _)| r.clone()).collect();

        // 2. The holders of the safe managed roles, and nothing else.
        let actual = if safe.is_empty() {
            BTreeMap::new()
        } else {
            match self.platform.holders(&safe).await {
                Ok(a) => a,
                Err(e) => {
                    let rep = self.read_failed(state, report, e, now);
                    self.send_alert(&rep, &new_refusals).await;
                    return rep;
                }
            }
        };
        let holds = match open_holds(&self.pool, p).await {
            Ok(h) => h,
            Err(e) => {
                report.error = Some(format!("database: {e}"));
                return report;
            }
        };

        // 3. Apply the difference.
        'roles: for role in &safe {
            let want_set: BTreeSet<String> = plan.members_of(role).into_iter().collect();
            let has_set = actual.get(role).cloned().unwrap_or_default();
            // Every CRA member on this platform, plus anyone else who holds the role: a
            // member's pair is settled even while they should not hold the role, so a hand
            // grant to them later is recognised as drift.
            let users: BTreeSet<&String> = want_set.iter().chain(has_set.iter()).chain(plan.users.iter()).collect();
            // Adds before removes, so a Slack user group is never emptied on the way.
            let mut users: Vec<&String> = users.into_iter().collect();
            users.sort_by_key(|u| !want_set.contains(*u) || has_set.contains(*u));
            for user in users {
                let pair = (role.clone(), user.clone());
                let want = want_set.contains(user);
                let has = has_set.contains(user);
                if want == has {
                    state.settled.insert(pair, has);
                    continue;
                }
                if holds.contains(&pair) {
                    state.settled.remove(&pair);
                    report.held.push(pair);
                    continue;
                }
                let drift = match state.settled.get(&pair) {
                    _ if mutant("no_drift") => false,
                    _ if mutant("slack_differs") && p == ChatPlatform::Slack => false,
                    Some(&prev) => prev == want,
                    None => has && !plan.users.contains(user) && !mutant("no_outsider_drift"),
                };
                let direction = if has { DriftDirection::Granted } else { DriftDirection::Removed };
                if drift {
                    match recent_reverts(&self.pool, p, role, user, now).await {
                        Ok(n) if n >= FLAP_LIMIT && !mutant("no_flap_limit") => {
                            state.settled.remove(&pair);
                            if let Err(e) = open_hold(&self.pool, p, role, user, now).await {
                                error!("role sync: could not record a hold: {e}");
                            }
                            let ev = SyncEvent {
                                platform: p,
                                kind: SyncEventKind::DriftHeld,
                                role: role.clone(),
                                member: user.clone(),
                                direction: Some(direction),
                                observed_at: now,
                                reverted_at: None,
                                snapshot_id,
                                detail: format!("{n} reverts within the anti-flap window; held"),
                            };
                            if let Err(e) = record_sync_event(&self.pool, &ev).await {
                                error!("role sync: could not record drift: {e}");
                            }
                            report.newly_held.push(Drift { role: role.clone(), user: user.clone(), direction });
                            continue;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            report.failed.push((role.clone(), user.clone(), format!("database: {e}")));
                            continue;
                        }
                    }
                }
                let res = if want {
                    self.platform.add_member(role, user).await
                } else {
                    self.platform.remove_member(role, user).await
                };
                match res {
                    Ok(()) => {
                        state.settled.insert(pair.clone(), want);
                        if drift {
                            let ev = SyncEvent {
                                platform: p,
                                kind: SyncEventKind::DriftReverted,
                                role: role.clone(),
                                member: user.clone(),
                                direction: Some(direction),
                                observed_at: now,
                                reverted_at: Some((self.clock)()),
                                snapshot_id,
                                detail: format!("{:?}", trigger),
                            };
                            if let Err(e) = record_sync_event(&self.pool, &ev).await {
                                error!("role sync: drift on {} reverted but not recorded: {e}", p.label());
                            }
                            report.reverted.push(Drift { role: role.clone(), user: user.clone(), direction });
                        } else {
                            let _ = record_sync_event(
                                &self.pool,
                                &SyncEvent {
                                    platform: p,
                                    kind: SyncEventKind::ChangeApplied,
                                    role: role.clone(),
                                    member: user.clone(),
                                    direction: None,
                                    observed_at: now,
                                    reverted_at: None,
                                    snapshot_id,
                                    detail: if want { "added".into() } else { "removed".into() },
                                },
                            )
                            .await;
                            if want {
                                report.added.push(pair);
                            } else {
                                report.removed.push(pair);
                            }
                        }
                    }
                    Err(_) if mutant("failure_counts_as_applied") => {
                        state.settled.insert(pair.clone(), want);
                        if want {
                            report.added.push(pair);
                        } else {
                            report.removed.push(pair);
                        }
                    }
                    Err(e) => {
                        // Never marked as applied: the next run sees the same difference.
                        state.settled.remove(&pair);
                        warn!("role sync: {} {} for {} on {} failed: {}", if want { "add" } else { "remove" }, role, user, p.label(), e);
                        report.failed.push((role.clone(), user.clone(), e.to_string()));
                        if let (false, PlatformError::RateLimited { retry_after_secs }) = (mutant("no_backoff"), e) {
                            report.rate_limited = true;
                            state.backoff_until =
                                Some(now + chrono::Duration::seconds(retry_after_secs.unwrap_or(DEFAULT_BACKOFF_SECS) as i64));
                            break 'roles;
                        }
                    }
                }
            }
        }

        self.send_alert(&report, &new_refusals).await;
        if report.applied() > 0 || !report.failed.is_empty() {
            info!(
                "role sync on {} ({:?}): {} applied, {} drift reverted, {} failed",
                p.label(),
                trigger,
                report.added.len() + report.removed.len(),
                report.reverted.len(),
                report.failed.len()
            );
        }
        report
    }

    fn read_failed(&self, state: &mut RunnerState, mut report: ReconcileReport, e: PlatformError, now: DateTime<Utc>) -> ReconcileReport {
        if let PlatformError::RateLimited { retry_after_secs } = &e {
            report.rate_limited = true;
            state.backoff_until = Some(now + chrono::Duration::seconds(retry_after_secs.unwrap_or(DEFAULT_BACKOFF_SECS) as i64));
        }
        warn!("role sync on {}: could not read the platform: {}", self.platform().label(), e);
        report.error = Some(format!("could not read {}: {e}", self.platform().label()));
        report
    }

    async fn send_alert(&self, report: &ReconcileReport, new_refusals: &[(String, RoleSafety)]) {
        if let Some(text) = report.alert_text(self.platform(), new_refusals) {
            if let Err(e) = self.platform.alert(&text).await {
                warn!("role sync: alert on {} not delivered: {}", self.platform().label(), e);
            }
        }
    }
}

// ---------------------------------------------------- triggers and the loop

/// Feeds triggers to one runner's background task.
#[derive(Clone)]
pub struct SyncHandle {
    tx: mpsc::UnboundedSender<Trigger>,
    runner: Arc<SyncRunner>,
}

impl SyncHandle {
    /// Queue a run. Triggers that arrive while a run is in progress coalesce into one run.
    pub fn trigger(&self, t: Trigger) {
        let _ = self.tx.send(t);
    }

    pub fn runner(&self) -> &Arc<SyncRunner> {
        &self.runner
    }
}

struct Registered {
    cra: CraHandle,
    tx: mpsc::UnboundedSender<Trigger>,
}

fn registry() -> &'static StdMutex<Vec<Registered>> {
    static R: OnceLock<StdMutex<Vec<Registered>>> = OnceLock::new();
    R.get_or_init(|| StdMutex::new(Vec::new()))
}

/// The two handles share state (one is a clone of the other).
fn same_cra(a: &CraHandle, b: &CraHandle) -> bool {
    match (a.active(), b.active()) {
        (Some(x), Some(y)) => Arc::ptr_eq(&x, &y),
        _ => false,
    }
}

/// The activation hook: `CraContext::load_and_apply` calls it after a snapshot is activated
/// and the runtime state swapped. Every runner on this CRA reconciles, and holds are
/// released.
pub fn snapshot_activated(cra: &CraHandle) {
    let mut reg = match registry().lock() {
        Ok(r) => r,
        Err(p) => p.into_inner(),
    };
    reg.retain(|r| !r.tx.is_closed());
    if mutant("no_activation_hook") {
        return;
    }
    for r in reg.iter().filter(|r| same_cra(&r.cra, cra)) {
        let _ = r.tx.send(Trigger::Activation);
    }
}

/// Start the background task for one runner: with a timer, a run at start and one per
/// `every`; with `every = None`, runs only on triggers. All runs go through this one task and the
/// runner's lock, so they never overlap.
pub fn spawn(runner: Arc<SyncRunner>, every: Option<Duration>) -> SyncHandle {
    let (tx, mut rx) = mpsc::unbounded_channel::<Trigger>();
    if let Ok(mut reg) = registry().lock() {
        reg.push(Registered { cra: runner.cra.clone(), tx: tx.clone() });
    }
    let task_runner = runner.clone();
    tokio::spawn(async move {
        let every = if mutant("no_timer") { None } else { every };
        let mut tick = every.map(|d| {
            let mut i = tokio::time::interval(d);
            i.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            i
        });
        loop {
            let first = tokio::select! {
                t = rx.recv() => match t { Some(t) => t, None => break },
                _ = async { match tick.as_mut() { Some(i) => { i.tick().await; } None => std::future::pending::<()>().await } } => Trigger::Timer,
            };
            // Coalesce everything already queued; the strongest trigger wins (an activation
            // releases holds, so it must not be lost behind a timer tick).
            let mut t = first;
            while let Ok(next) = rx.try_recv() {
                t = t.max(next);
            }
            let rep = task_runner.run(t).await;
            if let Some(e) = &rep.error {
                warn!("role sync on {} ({:?}): {}", task_runner.platform().label(), t, e);
            }
        }
    });
    SyncHandle { tx, runner }
}

/// `MSYNC_ROLE_SYNC_SECS` (default [`DEFAULT_ROLE_SYNC_SECS`]; 0 disables the timer).
pub fn interval_from_env() -> Option<Duration> {
    let secs = crate::config::env_var("MSYNC_ROLE_SYNC_SECS")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_ROLE_SYNC_SECS);
    (secs > 0).then(|| Duration::from_secs(secs))
}

#[cfg(test)]
#[path = "sync_tests.rs"]
pub(crate) mod tests;
