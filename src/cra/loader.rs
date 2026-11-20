//! Loading the CRA on a push (contract section 4) and the runtime state that
//! the engine's seams read.
//!
//! [`load_commit`] is the whole pipeline for one commit: fetch at the SHA,
//! [`super::load`], [`super::plan_change`] against the ACTIVE snapshot,
//! [`super::authorize`], then activate in one transaction or record a rejected
//! attempt with every violation. Every failure ends in a rejected row and an
//! unchanged active snapshot.
//!
//! [`CraHandle`] holds the active snapshot behind an `Arc` that is swapped on
//! activation. [`CraContext`] ties the store, the Foundry API source, the handle and the
//! engine owners together for the webhook handlers and the read API.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, RwLock};

use chrono::NaiveDate;
use serde::Serialize;
use sqlx::SqlitePool;
use tracing::{error, info, warn};
use uuid::Uuid;

use super::model::{Cadence, EventKind, MeritFile, OrgFile, PayoutDay, PayoutsFile, RepoVisibility, RunMode, Scorer};
use super::source::{normalize_sha, CraRepo, CraSource, SourceError};
use super::store::{self, Activation, Attempt, RecordedViolation, Recorded, SnapshotRow};
use super::{authorize, plan_change, ChangeContext, FoundryIdentity, Snapshot};
use crate::auth::Directory;
use crate::foundry::identity::{find_contributor, link_foundry_identity, resolve_contributor, LinkError};
use crate::foundry::{FoundryEvent, FoundryEventKind, FoundryProvider, RepoEntry, RepoRegistry};
use crate::merit::scoring::{EventRule, MeritRules, ScorerSource};
use crate::payments::policy as pay;

/// The engine org every snapshot belongs to (one CRA per deployment, the org of
/// every API token).
pub const ORG_ID: &str = crate::auth::DEFAULT_ORG;

/// Shown when a payout policy write is refused because the CRA is the author.
pub const POLICY_AUTHORED_IN_CRA: &str =
    "payout policy is authored in the CRA repository (.msync/payouts.toml) while a CRA snapshot is active; change it there through a reviewed commit";

// ------------------------------------------------------------------ outcome

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IdentityLinkReport {
    pub member_id: String,
    /// `provider:host:login`
    pub identity: String,
    /// `linked`, `conflict` or `error`.
    pub result: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum LoadOutcome {
    /// The commit is now the active snapshot.
    Activated {
        snapshot_id: i64,
        commit_sha: String,
        content_hash: String,
        plan_markdown: String,
        identity_links: Vec<IdentityLinkReport>,
    },
    /// Recorded as rejected; the active snapshot is unchanged.
    Rejected {
        snapshot_id: i64,
        commit_sha: String,
        retryable: bool,
        violations: Vec<RecordedViolation>,
    },
    /// The commit changes nothing in the CRA (empty plan). Nothing is recorded.
    NoChange { commit_sha: String },
    /// This commit already has a final outcome. Nothing happens.
    AlreadyProcessed { snapshot_id: i64, commit_sha: String, status: String },
}

impl LoadOutcome {
    pub fn label(&self) -> &'static str {
        match self {
            LoadOutcome::Activated { .. } => "activated",
            LoadOutcome::Rejected { .. } => "rejected",
            LoadOutcome::NoChange { .. } => "no_change",
            LoadOutcome::AlreadyProcessed { .. } => "already_processed",
        }
    }
}

/// The audit entry for a snapshot attempt (contract 13.1). The actor is the authenticated
/// pusher or merger; a reload with no webhook is `system:cra-reload`.
fn attempt_audit(a: &Attempt<'_>, action: &str, snapshot_id: i64, superseded: Option<i64>) -> crate::audit::AuditEntry {
    let (kind, actor) = match a.ctx {
        Some(c) => (crate::audit::ActorKind::Foundry, c.actor.to_string()),
        None => (crate::audit::ActorKind::System, "system:cra-reload".to_string()),
    };
    let mut e = crate::audit::AuditEntry::new(
        kind,
        actor,
        action,
        format!("cra_snapshot:{snapshot_id}"),
        serde_json::json!({
            "snapshot_id": snapshot_id,
            "repo": a.repo.spec(),
            "commit_sha": a.commit_sha,
            "org_slug": a.org_slug,
            "content_hash": a.files.map(|f| f.content_hash()),
            "via_pull_request": a.ctx.map(|c| c.via_pull_request),
            "author": a.ctx.and_then(|c| c.author.as_ref()).map(|x| x.to_string()),
            "approvers": a.ctx.map(|c| c.approvers.iter().map(|x| x.to_string()).collect::<Vec<_>>()),
            "superseded_snapshot_id": superseded,
            "violations": a.violations.iter().map(|v| serde_json::json!({"code": v.code, "item": v.item, "message": v.message})).collect::<Vec<_>>(),
            "retryable": a.retryable,
        }),
    );
    e.org_id = a.org_id.to_string();
    e
}

/// The audit entry `store::activate` writes in the activation transaction.
pub(crate) fn activation_audit(a: &Attempt<'_>, snapshot_id: i64, superseded: Option<i64>) -> crate::audit::AuditEntry {
    attempt_audit(a, "cra.snapshot.activated", snapshot_id, superseded)
}

fn source_violation(e: &SourceError) -> RecordedViolation {
    RecordedViolation::new("SourceError", "foundry", e.to_string())
}

fn already(row: &SnapshotRow) -> LoadOutcome {
    LoadOutcome::AlreadyProcessed { snapshot_id: row.id, commit_sha: row.commit_sha.clone(), status: row.status.clone() }
}

/// Rebuild the active snapshot from its stored files. The stored content hash must
/// match: a row that does not reproduce exactly is unusable (fail closed).
pub fn parse_stored(row: &SnapshotRow) -> Result<Snapshot, String> {
    let files = row.files.as_ref().ok_or("the active snapshot has no stored files")?;
    let snap = Snapshot::parse(files)
        .map_err(|e| e.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("; "))?;
    if row.content_hash.as_deref() != Some(snap.content_hash.as_str()) {
        return Err("the stored content hash does not match the stored files".into());
    }
    Ok(snap)
}

/// Load one commit of the CRA repository.
///
/// `pusher` is the authenticated pusher from a verified push webhook (None for a
/// reload). `owners` is engine configuration (`MSYNC_OWNER_MEMBER_IDS`).
///
/// Order: SHA check, idempotency, fetch files at the SHA, change context, parse and
/// validate, plan against the active snapshot, authorize against the active snapshot,
/// activate. A database error is returned as `Err` and writes nothing partial.
pub async fn load_commit(
    pool: &SqlitePool,
    source: &dyn CraSource,
    cra_repo: &CraRepo,
    sha: &str,
    pusher: Option<&FoundryIdentity>,
    owners: &[String],
    today: NaiveDate,
) -> Result<LoadOutcome, sqlx::Error> {
    let org_id = ORG_ID;
    let reject = |sha: String,
                  slug: Option<String>,
                  files: Option<super::CraFiles>,
                  ctx: Option<ChangeContext>,
                  violations: Vec<RecordedViolation>,
                  plan: Option<String>,
                  retryable: bool| async move {
        for v in &violations {
            warn!("CRA commit {} rejected: {}: {}: {}", sha, v.code, v.item, v.message);
        }
        let a = Attempt {
            org_id,
            org_slug: slug.as_deref(),
            repo: cra_repo,
            commit_sha: &sha,
            files: files.as_ref(),
            ctx: ctx.as_ref(),
            violations: &violations,
            plan_markdown: plan.as_deref(),
            retryable,
        };
        Ok::<LoadOutcome, sqlx::Error>(match store::record_rejected(pool, &a).await? {
            Recorded::Inserted(id) => {
                // A rejection changes nothing, so its audit entry is best effort (logged on failure).
                crate::audit::append_best_effort(pool, &attempt_audit(&a, "cra.snapshot.rejected", id, None)).await;
                LoadOutcome::Rejected { snapshot_id: id, commit_sha: sha.clone(), retryable, violations }
            }
            Recorded::AlreadyFinal(row) => already(&row),
        })
    };

    // 0. The source must be the configured CRA repository.
    if source.repo() != cra_repo {
        let v = RecordedViolation::new("SourceMismatch", "source", format!("source reads {} but the CRA is {}", source.repo().spec(), cra_repo.spec()));
        return reject(sha.trim().to_ascii_lowercase(), None, None, None, vec![v], None, false).await;
    }

    // 1. An exact commit, never a branch or tag name.
    let sha = match normalize_sha(sha) {
        Ok(s) => s,
        Err(e) => {
            let v = RecordedViolation::new("InvalidCommitSha", "commit", e.to_string());
            return reject(sha.trim().to_ascii_lowercase(), None, None, None, vec![v], None, false).await;
        }
    };

    // 2. Idempotent per (org, commit).
    if let Some(row) = store::final_for_commit(pool, org_id, &sha).await? {
        info!("CRA commit {} already {}; nothing to do", sha, row.status);
        return Ok(already(&row));
    }

    // 3. The files, from the Foundry API at the SHA.
    let files = match source.fetch_files(&sha).await {
        Ok(f) => f,
        Err(e) => return reject(sha, None, None, None, vec![source_violation(&e)], None, true).await,
    };

    // 4. Who made the change and how it arrived.
    let ctx = match source.change_context(&sha, pusher, today).await {
        Ok(c) => c,
        Err(e) => return reject(sha, None, Some(files), None, vec![source_violation(&e)], None, true).await,
    };

    // 5. Parse and validate.
    let new = match super::load(&files, today) {
        Ok(s) => s,
        Err(errors) => {
            let slug = Snapshot::parse(&files).ok().map(|s| s.org.org.slug);
            let vs = errors
                .iter()
                .map(|e| RecordedViolation::new(format!("{:?}", e.code), e.file.file_name(), e.message.clone()))
                .collect();
            return reject(sha, slug, Some(files), Some(ctx), vs, None, false).await;
        }
    };
    let slug = Some(new.org.org.slug.clone());

    // 6. Plan against the ACTIVE snapshot.
    let active_row = store::active(pool, org_id).await?;
    let old = match &active_row {
        None => None,
        Some(row) => match parse_stored(row) {
            Ok(s) => Some(s),
            Err(msg) => {
                let v = RecordedViolation::new("ActiveSnapshotUnusable", "active", msg);
                return reject(sha, slug, Some(files), Some(ctx), vec![v], None, true).await;
            }
        },
    };
    let plan = plan_change(old.as_ref(), &new);
    if plan.is_empty() {
        info!("CRA commit {} changes nothing in .msync/; no-op", sha);
        return Ok(LoadOutcome::NoChange { commit_sha: sha });
    }
    let plan_md = plan.render_markdown();

    // 7. Authorize against the ACTIVE snapshot. A change cannot authorize itself.
    if let Err(violations) = authorize(old.as_ref(), &plan, &ctx, owners) {
        let vs = violations
            .iter()
            .map(|v| RecordedViolation::new(format!("{:?}", v.code), v.item.clone(), v.message.clone()))
            .collect();
        return reject(sha, slug, Some(files), Some(ctx), vs, Some(plan_md), false).await;
    }

    // 8. Activate: old active becomes superseded, new becomes active, in one transaction.
    let attempt = Attempt {
        org_id,
        org_slug: slug.as_deref(),
        repo: cra_repo,
        commit_sha: &sha,
        files: Some(&files),
        ctx: Some(&ctx),
        violations: &[],
        plan_markdown: Some(&plan_md),
        retryable: false,
    };
    match store::activate(pool, &attempt, active_row.as_ref().map(|r| r.id)).await? {
        Activation::Activated(id) => {
            info!("CRA commit {} activated as snapshot {} by {}", sha, id, ctx.actor);
            let identity_links = link_member_identities(pool, &new).await;
            // Contract 12.3: a member this snapshot suspends or removes loses their OAuth tokens.
            match crate::oauth::revoke_inactive_member_grants(pool, &new).await {
                Ok(0) => {}
                Ok(n) => info!("CRA snapshot {}: revoked {} OAuth token families of members who are not active", id, n),
                Err(e) => error!("CRA snapshot {}: revoking OAuth tokens of inactive members failed: {}", id, e),
            }
            Ok(LoadOutcome::Activated {
                snapshot_id: id,
                commit_sha: sha,
                content_hash: new.content_hash.clone(),
                plan_markdown: plan_md,
                identity_links,
            })
        }
        Activation::AlreadyFinal(row) => Ok(already(&row)),
        Activation::ActiveChanged { current } => {
            let v = RecordedViolation::new(
                "ActiveChanged",
                "active",
                format!("the active snapshot changed to {current:?} while this commit was being checked; it was not activated"),
            );
            reject(sha, slug, Some(files), Some(ctx), vec![v], Some(plan_md), true).await
        }
    }
}

// --------------------------------------------------------------- identities

fn engine_provider(p: super::FoundryProvider) -> FoundryProvider {
    match p {
        super::FoundryProvider::Github => FoundryProvider::GitHub,
        super::FoundryProvider::Gitlab => FoundryProvider::GitLab,
    }
}

/// Hosts a provider's identities are valid on: the hosts of that provider in
/// `org.toml` `[[foundry]]`, or the provider's public host when none is listed.
fn hosts_for(org: &OrgFile, provider: FoundryProvider) -> Vec<String> {
    let mut hosts: BTreeSet<String> = org
        .foundries
        .iter()
        .filter(|f| engine_provider(f.provider) == provider)
        .map(|f| f.host.clone().unwrap_or_else(|| provider.default_host().to_string()).to_ascii_lowercase())
        .collect();
    if hosts.is_empty() {
        hosts.insert(provider.default_host().to_string());
    }
    hosts.into_iter().collect()
}

/// Every `(provider, host, login)` a member is attested to hold.
pub fn member_foundry_logins(snap: &Snapshot, member_id: &str) -> Vec<(FoundryProvider, String, String)> {
    let Some(m) = snap.member(member_id) else { return Vec::new() };
    let mut out = Vec::new();
    for (p, login) in [(FoundryProvider::GitHub, &m.identities.github), (FoundryProvider::GitLab, &m.identities.gitlab)] {
        if let Some(login) = login {
            for host in hosts_for(&snap.org, p) {
                out.push((p, host, login.clone()));
            }
        }
    }
    out
}

/// On activation: attach every member's GitHub and GitLab identities to one contributor so their
/// XP adds up. An identity that already belongs to another contributor is reported as a
/// conflict and never merged.
pub async fn link_member_identities(pool: &SqlitePool, snap: &Snapshot) -> Vec<IdentityLinkReport> {
    let mut reports = Vec::new();
    for m in &snap.members.members {
        let logins = member_foundry_logins(snap, &m.id);
        if logins.is_empty() {
            continue;
        }
        let mut primary: Option<Uuid> = None;
        for (p, host, login) in &logins {
            match find_contributor(pool, *p, host, login).await {
                Ok(Some(c)) => {
                    primary = Some(c.id);
                    break;
                }
                Ok(None) => {}
                Err(e) => {
                    error!("identity lookup for {} failed: {}", m.id, e);
                }
            }
        }
        let primary = match primary {
            Some(id) => id,
            None => {
                let (p, host, login) = &logins[0];
                match resolve_contributor(pool, *p, host, login).await {
                    Ok(c) => c.id,
                    Err(e) => {
                        reports.push(IdentityLinkReport {
                            member_id: m.id.clone(),
                            identity: format!("{p}:{host}:{login}"),
                            result: "error".into(),
                            message: e.to_string(),
                        });
                        continue;
                    }
                }
            }
        };
        for (p, host, login) in &logins {
            let label = format!("{p}:{host}:{}", login.to_ascii_lowercase());
            let (result, message) = match link_foundry_identity(pool, primary, *p, host, login).await {
                Ok(()) => ("linked", String::new()),
                Err(LinkError::Conflict(x)) => {
                    warn!("CRA member {}: identity {} belongs to another contributor; not merged", m.id, x);
                    ("conflict", format!("{x} already belongs to another contributor; not merged"))
                }
                Err(e) => ("error", e.to_string()),
            };
            reports.push(IdentityLinkReport { member_id: m.id.clone(), identity: label, result: result.into(), message });
        }
    }
    reports
}

/// A member's merit: the total of every contributor their attested identities map to.
/// All merit counts as vested today.
pub async fn member_merit(pool: &SqlitePool, snap: &Snapshot, member_id: &str) -> Result<u64, sqlx::Error> {
    let mut seen = BTreeSet::new();
    let mut total: u64 = 0;
    for (p, host, login) in member_foundry_logins(snap, member_id) {
        if let Some(c) = find_contributor(pool, p, &host, &login).await? {
            if seen.insert(c.id) {
                total = total.saturating_add(c.total_merit_points.max(0) as u64);
            }
        }
    }
    Ok(total)
}

// ------------------------------------------------------------------ runtime

/// The active snapshot as the engine uses it.
#[derive(Debug, Clone)]
pub struct ActiveCra {
    pub snapshot_id: i64,
    pub commit_sha: String,
    pub loaded_at: String,
    pub actor: Option<String>,
    pub repo: CraRepo,
    pub snapshot: Snapshot,
}

/// The swappable active snapshot plus a merit cache for the synchronous
/// [`Directory`]. Cloning shares the same state.
#[derive(Clone, Default)]
pub struct CraHandle {
    active: Arc<RwLock<Option<Arc<ActiveCra>>>>,
    merits: Arc<RwLock<HashMap<String, u64>>>,
}

impl std::fmt::Debug for CraHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CraHandle").field("active", &self.active().map(|a| a.snapshot_id)).finish()
    }
}

impl CraHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// The active snapshot, if any.
    pub fn active(&self) -> Option<Arc<ActiveCra>> {
        self.active.read().map(|g| g.clone()).unwrap_or(None)
    }

    pub fn replace(&self, v: Option<Arc<ActiveCra>>) {
        if let Ok(mut g) = self.active.write() {
            *g = v;
        }
    }

    /// Cached vested merit of a CRA member (0 when unknown).
    pub fn merit_of(&self, member_id: &str) -> u64 {
        self.merits.read().ok().and_then(|m| m.get(member_id).copied()).unwrap_or(0)
    }

    /// Load the active snapshot from the store. A stored active row that does not
    /// reproduce is an error: the caller must not run on a guess.
    pub async fn reload_from_store(&self, pool: &SqlitePool, org_id: &str) -> Result<(), String> {
        let row = store::active(pool, org_id).await.map_err(|e| e.to_string())?;
        let next = match row {
            None => None,
            Some(row) => {
                let snapshot = parse_stored(&row).map_err(|e| format!("active CRA snapshot {} is unusable: {e}", row.id))?;
                let repo = CraRepo::new(
                    FoundryProvider::parse(&row.provider).ok_or("active CRA snapshot has an unknown provider")?,
                    &row.host,
                    &row.repo,
                );
                Some(Arc::new(ActiveCra {
                    snapshot_id: row.id,
                    commit_sha: row.commit_sha.clone(),
                    loaded_at: row.loaded_at.clone(),
                    actor: row.actor.clone(),
                    repo,
                    snapshot,
                }))
            }
        };
        self.replace(next);
        self.refresh_merits(pool).await.map_err(|e| e.to_string())
    }

    /// Recompute the merit cache for every member of the active snapshot.
    pub async fn refresh_merits(&self, pool: &SqlitePool) -> Result<(), sqlx::Error> {
        let mut next = HashMap::new();
        if let Some(a) = self.active() {
            for m in &a.snapshot.members.members {
                next.insert(m.id.clone(), member_merit(pool, &a.snapshot, &m.id).await?);
            }
        }
        if let Ok(mut g) = self.merits.write() {
            *g = next;
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn set_merit_for_test(&self, member_id: &str, merit: u64) {
        self.merits.write().unwrap().insert(member_id.to_string(), merit);
    }
}

// ------------------------------------------------------------------- seams

/// `org.toml` `[[foundry]]` as the repository registry: canonical and visibility as listed.
pub fn registry_from_cra(org: &OrgFile) -> RepoRegistry {
    RepoRegistry::from_entries(
        org.foundries
            .iter()
            .map(|f| {
                let provider = engine_provider(f.provider);
                RepoEntry {
                    provider,
                    host: f.host.clone().unwrap_or_else(|| provider.default_host().to_string()).trim().to_ascii_lowercase(),
                    path: f.repo.trim().trim_matches('/').to_ascii_lowercase(),
                    canonical: f.canonical,
                    private: f.visibility == RepoVisibility::Private,
                }
            })
            .collect(),
    )
}

/// `merit.toml` as merit rules. `pr_merged` and `review_approved` take their points
/// from the matching `[[event]]`; an event the file does not list earns nothing.
/// Opening a pull request is activity, not landed work, and has no CRA event: it
/// earns nothing while a snapshot is active. `weight`, `daily_cap`, `vesting_days`
/// and `external_counts` are carried by the CRA but not yet enforced by the merit path.
pub fn merit_rules_from_cra(m: &MeritFile) -> MeritRules {
    let rule = |k: EventKind| {
        m.events
            .iter()
            .find(|e| e.key == k)
            .map(|e| EventRule { base_points: e.base_points.min(i32::MAX as u32) as i32, max_points: e.max_points.min(i32::MAX as u32) as i32 })
            .unwrap_or(EventRule { base_points: 0, max_points: 0 })
    };
    MeritRules {
        rubric: m.rubric.clone(),
        scorers: m
            .scorers
            .iter()
            .map(|s| match s {
                Scorer::Agent => ScorerSource::Agent,
                Scorer::Coderabbit => ScorerSource::CodeRabbit,
            })
            .collect(),
        min_scorers: m.min_scorers as usize,
        score_window_days: i64::from(m.score_window_days),
        pr_opened: EventRule { base_points: 0, max_points: 0 },
        pr_merged: rule(EventKind::PrMerged),
        review_approved: rule(EventKind::ReviewApproved),
    }
}

/// The one conversion from the CRA's `payouts.toml` to the engine's payout policy.
/// The result is validated; any error refuses the policy.
pub fn payout_policy_from_cra(p: &PayoutsFile) -> Result<pay::PayoutPolicy, Vec<String>> {
    let to_i64 = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    let policy = pay::PayoutPolicy {
        schema: pay::POLICY_SCHEMA,
        enabled: p.enabled,
        currency: p.currency.clone(),
        schedule: pay::Schedule {
            cadence: match p.schedule.cadence {
                Cadence::Manual => pay::Cadence::Manual,
                Cadence::Weekly => pay::Cadence::Weekly,
                Cadence::Biweekly => pay::Cadence::Biweekly,
                Cadence::Monthly => pay::Cadence::Monthly,
            },
            day: p.schedule.day.as_ref().map(|d| match d {
                PayoutDay::DayOfMonth(n) => pay::ScheduleDay::MonthDay(i64::from(*n)),
                PayoutDay::Weekday(w) => pay::ScheduleDay::Weekday(w.clone()),
            }),
            hour_utc: Some(i64::from(p.schedule.hour_utc)),
            mode: match p.schedule.run_mode {
                RunMode::Prepare => pay::ScheduleMode::Prepare,
                RunMode::Auto => pay::ScheduleMode::Auto,
            },
        },
        limits: pay::Limits {
            min_payout_cents: Some(to_i64(p.limits.min_payout_cents)),
            max_per_payout_cents: Some(to_i64(p.limits.max_per_payout_cents)),
            max_per_member_per_period_cents: Some(to_i64(p.limits.max_per_member_per_period_cents)),
            max_per_run_cents: Some(to_i64(p.limits.max_per_run_cents)),
            hold_days: Some(i64::from(p.limits.hold_days)),
        },
        approval: pay::Approval {
            approvals_required: i64::from(p.approval.approvals_required),
            min_approver_rank: i64::from(p.approval.min_approver_rank),
        },
        eligibility: pay::Eligibility {
            min_rank: i64::from(p.eligibility.min_rank),
            roles: p.eligibility.roles.clone(),
            require_verified_identity: p.eligibility.require_verified_identity,
        },
        rate: p
            .rates
            .iter()
            .map(|r| pay::Rate { branch: r.branch.clone(), base_cents: to_i64(r.base_cents), merit_multiplier: r.merit_multiplier })
            .collect(),
        time_auto_approve: p.time_auto_approve,
    };
    let errs = policy.validate();
    if errs.is_empty() {
        Ok(policy)
    } else {
        Err(errs)
    }
}

/// The payout policy from the active snapshot of `org`, read from the store. `Ok(None)`
/// when no snapshot is active. A stored `payouts.toml` that does not parse or convert is
/// `Corrupt` (fail closed: never fall back to another policy).
pub async fn cra_payout_policy(pool: &SqlitePool, org: &str) -> Result<Option<pay::EffectivePolicy>, pay::PolicyError> {
    let Some(row) = store::active(pool, org).await? else { return Ok(None) };
    let text = row
        .files
        .as_ref()
        .map(|f| f.payouts.clone())
        .ok_or_else(|| pay::PolicyError::Corrupt(format!("CRA snapshot {}: no payouts.toml stored", row.id)))?;
    let file: PayoutsFile = toml::from_str(&text)
        .map_err(|e| pay::PolicyError::Corrupt(format!("CRA snapshot {}: payouts.toml: {e}", row.id)))?;
    let policy = payout_policy_from_cra(&file)
        .map_err(|e| pay::PolicyError::Corrupt(format!("CRA snapshot {}: {}", row.id, e.join("; "))))?;
    Ok(Some(pay::EffectivePolicy {
        policy,
        version: Some(row.id),
        updated_by: Some(format!("cra:{}", row.commit_sha)),
        updated_at: Some(row.loaded_at),
    }))
}

/// Is a CRA snapshot active for `org`? (The store is the source of truth.)
pub async fn cra_active(pool: &SqlitePool, org: &str) -> Result<bool, sqlx::Error> {
    Ok(store::active_count(pool, org).await? > 0)
}

// ------------------------------------------------------------------ context

/// The CRA repository and branch (`MSYNC_CRA_REPO`, `MSYNC_CRA_BRANCH`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CraSettings {
    pub repo: CraRepo,
    pub branch: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CraRuntimeError {
    #[error("no CRA repository is configured (MSYNC_CRA_REPO)")]
    NotConfigured,
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("{0}")]
    State(String),
}

/// Everything CRA loading and the hierarchy API need.
pub struct CraContext {
    pub pool: SqlitePool,
    pub handle: CraHandle,
    pub directory: Arc<dyn Directory>,
    /// Engine owners (`MSYNC_OWNER_MEMBER_IDS` entries: member ids and `provider:login`).
    pub owners: Vec<String>,
    pub settings: Option<CraSettings>,
    pub source: Option<Arc<dyn CraSource>>,
    load_lock: tokio::sync::Mutex<()>,
}

impl CraContext {
    pub fn new(
        pool: SqlitePool,
        handle: CraHandle,
        directory: Arc<dyn Directory>,
        owners: Vec<String>,
        settings: Option<CraSettings>,
        source: Option<Arc<dyn CraSource>>,
    ) -> Self {
        CraContext { pool, handle, directory, owners, settings, source, load_lock: tokio::sync::Mutex::new(()) }
    }

    /// A context with no CRA repository: everything behaves as before CRA loading.
    pub fn disabled(pool: SqlitePool, handle: CraHandle, directory: Arc<dyn Directory>) -> Self {
        Self::new(pool, handle, directory, Vec::new(), None, None)
    }

    /// Is this verified push for the CRA repository's configured branch?
    pub fn is_cra_push(&self, ev: &FoundryEvent) -> bool {
        let Some(s) = &self.settings else { return false };
        ev.kind == FoundryEventKind::Push
            && ev.repo.as_deref().is_some_and(|r| s.repo.matches(ev.provider, &ev.host, r))
            && ev.base_branch.as_deref() == Some(s.branch.as_str())
            && ev.merge_sha.is_some()
    }

    /// Run the loader for one commit and, on activation, swap the runtime state.
    pub async fn load_and_apply(&self, sha: &str, pusher: Option<&FoundryIdentity>, today: NaiveDate) -> Result<LoadOutcome, CraRuntimeError> {
        let settings = self.settings.as_ref().ok_or(CraRuntimeError::NotConfigured)?;
        let source = self.source.as_ref().ok_or(CraRuntimeError::NotConfigured)?;
        let _guard = self.load_lock.lock().await;
        let outcome = load_commit(&self.pool, source.as_ref(), &settings.repo, sha, pusher, &self.owners, today).await?;
        if matches!(outcome, LoadOutcome::Activated { .. }) {
            self.handle.reload_from_store(&self.pool, ORG_ID).await.map_err(CraRuntimeError::State)?;
            crate::chat::sync::snapshot_activated(&self.handle);
        }
        Ok(outcome)
    }

    /// The push webhook trigger. `None` when the push is not for the CRA repository's
    /// configured branch: nothing is loaded.
    pub async fn on_push(&self, ev: &FoundryEvent, today: NaiveDate) -> Option<Result<LoadOutcome, CraRuntimeError>> {
        if !self.is_cra_push(ev) {
            return None;
        }
        let sha = ev.merge_sha.clone()?;
        let pusher = FoundryIdentity {
            provider: match ev.provider {
                FoundryProvider::GitHub => super::FoundryProvider::Github,
                FoundryProvider::GitLab => super::FoundryProvider::Gitlab,
            },
            login: ev.actor_login.clone(),
        };
        Some(self.load_and_apply(&sha, Some(&pusher), today).await)
    }

    /// Re-run the loader on the configured branch head (no webhook pusher).
    pub async fn reload_head(&self, today: NaiveDate) -> Result<LoadOutcome, CraRuntimeError> {
        let settings = self.settings.as_ref().ok_or(CraRuntimeError::NotConfigured)?;
        let source = self.source.as_ref().ok_or(CraRuntimeError::NotConfigured)?;
        let head = source.branch_head(&settings.branch).await?;
        self.load_and_apply(&head, None, today).await
    }

    /// Refresh the merit cache when a snapshot is active (after merit changes).
    pub async fn refresh_merits(&self) {
        if self.handle.active().is_some() {
            if let Err(e) = self.handle.refresh_merits(&self.pool).await {
                error!("CRA merit cache refresh failed: {}", e);
            }
        }
    }
}

/// The CRA repository and branch from configuration, if one is configured.
pub fn settings_from_config(cfg: &crate::config::Config) -> Option<CraSettings> {
    cfg.cra.repo.clone().map(|repo| CraSettings { repo, branch: cfg.cra.branch.clone() })
}

/// The Foundry API source for the configured CRA repository: GitHub with `GITHUB_TOKEN`,
/// GitLab with `GITLAB_TOKEN`.
pub fn source_from_config(cfg: &crate::config::Config) -> Option<Arc<dyn CraSource>> {
    let repo = cfg.cra.repo.clone()?;
    Some(match repo.provider {
        FoundryProvider::GitHub => Arc::new(super::source::GitHubSource::new(
            repo,
            cfg.cra.github_api_base.clone(),
            Some(cfg.github.token.clone()).filter(|t| !t.trim().is_empty()),
        )) as Arc<dyn CraSource>,
        FoundryProvider::GitLab => Arc::new(super::source::GitLabSource::new(
            repo,
            cfg.cra.gitlab_api_base.clone(),
            cfg.cra.gitlab_token.clone(),
        )),
    })
}

/// The local CRA directory (`MSYNC_CRA_LOCAL_DIR`, dev mode only; contract 13.5) as CRA
/// settings and a source. `Ok(None)` when no local directory is configured. Refused when
/// dev mode is off (checked again here, not only in `Config`) or when no owner entry has
/// the `provider:login` form the local load acts as.
#[allow(clippy::type_complexity)]
pub fn local_from_config(
    cfg: &crate::config::Config,
    owners: &[String],
) -> Result<Option<(CraSettings, Arc<dyn CraSource>)>, String> {
    let Some(dir) = cfg.cra.local_dir.clone() else { return Ok(None) };
    if !cfg.dev_mode {
        return Err("MSYNC_CRA_LOCAL_DIR is refused unless MSYNC_DEV_MODE=true".into());
    }
    let src = super::local::LocalDirSource::new(dir.clone(), owners).ok_or_else(|| {
        format!(
            "MSYNC_CRA_LOCAL_DIR={} needs an owner of the form github:<login> or gitlab:<login> in MSYNC_OWNER_MEMBER_IDS: the local load acts as that owner",
            dir.display()
        )
    })?;
    let settings = CraSettings { repo: src.repo().clone(), branch: super::local::LOCAL_BRANCH.to_string() };
    Ok(Some((settings, Arc::new(src) as Arc<dyn CraSource>)))
}

/// Refresh the CRA merit cache every `every` (background task).
pub async fn run_merit_refresher(ctx: Arc<CraContext>, every: std::time::Duration) {
    let mut tick = tokio::time::interval(every);
    loop {
        tick.tick().await;
        ctx.refresh_merits().await;
    }
}

/// Member ids mapped to their cached merit, for projections.
pub fn cached_merits(handle: &CraHandle) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    if let Some(a) = handle.active() {
        for m in &a.snapshot.members.members {
            out.insert(m.id.clone(), handle.merit_of(&m.id));
        }
    }
    out
}

#[cfg(test)]
#[path = "loader_tests.rs"]
mod tests;
