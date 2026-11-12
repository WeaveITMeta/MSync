//! The merit and payout path, run on a normalized [`ForgeEvent`] from any forge.
//!
//! Every effect is claimed once through `processed_webhook_events` with the event's
//! `effect_key` (released if applying fails, so a redelivery can retry). The payout
//! ledger is written through `payments::ledger::record_merge`, unchanged.

use super::identity::resolve_contributor;
use super::{ForgeEvent, ForgeEventKind, ForgeProvider, RepoDecision, RepoRef, RepoRegistry, Subject};
use crate::github::dedupe;
use crate::merit::scoring::{self, MeritRules};
use crate::models::{ActivityType, ContributionStatus, Contributor};
use crate::payments::{ledger, PaymentConfig};
use anyhow::Result;
use chrono::Utc;
use sqlx::SqlitePool;
use std::sync::Arc;
use tracing::{info, warn};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq)]
pub enum ProcessOutcome {
    /// Merit applied (points awarded by this call; 0 when only the payout path ran).
    Applied { points: i32 },
    /// Effect already processed earlier.
    Duplicate,
    /// Repository may not earn (unlisted, mirror, provider refused) or the event cannot be attributed.
    Ignored(String),
}

#[derive(Clone)]
pub struct ForgeProcessor {
    pool: SqlitePool,
    payments: Option<Arc<PaymentConfig>>,
    registry: Arc<RepoRegistry>,
    rules: Arc<MeritRules>,
}

impl ForgeProcessor {
    /// Legacy registry (GitHub only), default merit rules, no payments.
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            payments: None,
            registry: Arc::new(RepoRegistry::legacy()),
            rules: Arc::new(MeritRules::default()),
        }
    }

    pub fn with_payments(mut self, cfg: Arc<PaymentConfig>) -> Self {
        self.payments = Some(cfg);
        self
    }

    pub fn with_registry(mut self, registry: Arc<RepoRegistry>) -> Self {
        self.registry = registry;
        self
    }

    pub fn with_rules(mut self, rules: Arc<MeritRules>) -> Self {
        self.rules = rules;
        self
    }

    pub fn registry(&self) -> &RepoRegistry {
        &self.registry
    }

    /// Would this event's repository earn? (Checked before any state is written.)
    pub fn decide(&self, ev: &ForgeEvent) -> RepoDecision {
        self.registry.decide(ev.provider, &ev.host, ev.repo.as_deref())
    }

    pub async fn process(&self, ev: &ForgeEvent) -> Result<ProcessOutcome> {
        match self.decide(ev) {
            RepoDecision::Earn => {}
            other => {
                info!("{} event for {:?} on {} ignored: {:?}", ev.provider, ev.repo, ev.host, other);
                return Ok(ProcessOutcome::Ignored(format!("{:?}", other)));
            }
        }
        match ev.kind {
            ForgeEventKind::PrOpened => self.pr_opened(ev).await,
            ForgeEventKind::PrMerged => self.pr_merged(ev).await,
            ForgeEventKind::ReviewApproved => self.review_approved(ev).await,
            other => Ok(ProcessOutcome::Ignored(format!("{:?} earns nothing yet", other))),
        }
    }

    /// Run `apply` (a lazy future: nothing runs until awaited) under the effect key;
    /// release the key if it fails.
    async fn once(&self, key: &str, apply: impl std::future::Future<Output = Result<i32>>) -> Result<Option<i32>> {
        if !dedupe::claim(&self.pool, key).await? {
            return Ok(None);
        }
        match apply.await {
            Ok(p) => Ok(Some(p)),
            Err(e) => {
                let _ = dedupe::release(&self.pool, key).await;
                Err(e)
            }
        }
    }

    async fn pr_opened(&self, ev: &ForgeEvent) -> Result<ProcessOutcome> {
        let Some(author) = ev.author_login.clone() else {
            return Ok(ProcessOutcome::Ignored("author unknown".into()));
        };
        info!("Handling opened {} #{} by {}", ev.provider, ev.subject_number, author);
        let res = self.once(&ev.effect_key, self.apply_opened(ev, &author)).await?;
        Ok(match res {
            Some(points) => ProcessOutcome::Applied { points },
            None => {
                info!("PR #{} open already processed; skipping", ev.subject_number);
                ProcessOutcome::Duplicate
            }
        })
    }

    async fn pr_merged(&self, ev: &ForgeEvent) -> Result<ProcessOutcome> {
        let subject = ev.subject();
        let author = match ev.author_login.clone() {
            Some(a) => Some(a),
            None => self.recorded_author(subject.as_ref()).await?,
        };
        let Some(author) = author else {
            warn!("{} #{} merged but its author is unknown; no merit, no ledger row", ev.provider, ev.subject_number);
            return Ok(ProcessOutcome::Ignored("author unknown".into()));
        };
        info!("Handling merged {} #{} by {}", ev.provider, ev.subject_number, author);

        let res = self.once(&ev.effect_key, self.apply_merged(ev, subject.as_ref(), &author)).await?;

        let (merit_points, outcome) = match res {
            Some(points) => (points, ProcessOutcome::Applied { points }),
            None => {
                info!("PR #{} merge already processed; merit not re-awarded", ev.subject_number);
                (self.recorded_merit(ev).await?, ProcessOutcome::Duplicate)
            }
        };
        let username = resolve_contributor(&self.pool, ev.provider, &ev.host, &author).await?.github_username;
        self.record_payout(ev, &username, merit_points).await?;
        Ok(outcome)
    }

    async fn review_approved(&self, ev: &ForgeEvent) -> Result<ProcessOutcome> {
        let reviewer = ev.actor_login.clone();
        if ev.provider == ForgeProvider::GitLab {
            // GitLab can allow authors to approve their own MR; that earns nothing.
            let author = match ev.author_login.clone() {
                Some(a) => Some(a),
                None => self.recorded_author(ev.subject().as_ref()).await?,
            };
            if author.map(|a| a.eq_ignore_ascii_case(&reviewer)).unwrap_or(false) {
                return Ok(ProcessOutcome::Ignored("self-approval".into()));
            }
        }
        let res = self.once(&ev.effect_key, self.apply_review(ev, &reviewer)).await?;
        Ok(match res {
            Some(points) => ProcessOutcome::Applied { points },
            None => {
                info!("Review approval by {} already awarded; skipping", reviewer);
                ProcessOutcome::Duplicate
            }
        })
    }

    async fn apply_opened(&self, ev: &ForgeEvent, author: &str) -> Result<i32> {
        let contributor = resolve_contributor(&self.pool, ev.provider, &ev.host, author).await?;
        self.insert_contribution(ev, &contributor, author, ContributionStatus::Pending, 0, None).await?;
        let points = self.rules.pr_opened.points(None);
        self.record_activity(ev, contributor.id, ActivityType::PullRequestOpened, points,
            format!("Opened PR #{}: {}", ev.subject_number, ev.title)).await?;
        self.update_contributor_points(contributor.id, points).await?;
        Ok(points)
    }

    async fn apply_merged(&self, ev: &ForgeEvent, subject: Option<&Subject>, author: &str) -> Result<i32> {
        let contributor = resolve_contributor(&self.pool, ev.provider, &ev.host, author).await?;
        // The engine never invents a score: a score counts only if scorers already submitted one.
        let score = match subject {
            Some(s) => scoring::aggregate_for_rules(&self.pool, s, &self.rules).await?,
            None => None,
        };
        let points = self.rules.pr_merged.points(score);
        self.mark_merged(ev, &contributor, author, points, score).await?;
        if let (Some(s), Some(_)) = (subject, score) {
            // Score already priced in at merge: a later adjustment must not apply on top.
            dedupe::claim(&self.pool, &scoring::score_adjust_key(s)).await?;
        }
        self.record_activity(ev, contributor.id, ActivityType::PullRequestMerged, points,
            format!("Merged PR #{}: {} (Score: {:?})", ev.subject_number, ev.title, score)).await?;
        self.update_contributor_points(contributor.id, points).await?;
        Ok(points)
    }

    async fn apply_review(&self, ev: &ForgeEvent, reviewer: &str) -> Result<i32> {
        let c = resolve_contributor(&self.pool, ev.provider, &ev.host, reviewer).await?;
        let points = self.rules.review_approved.points(None);
        self.record_activity(ev, c.id, ActivityType::CodeReview, points, "Approved a pull request review".to_string()).await?;
        self.update_contributor_points(c.id, points).await?;
        Ok(points)
    }

    async fn recorded_author(&self, subject: Option<&Subject>) -> Result<Option<String>> {
        let Some(s) = subject else { return Ok(None) };
        Ok(sqlx::query_scalar::<_, Option<String>>(
            "SELECT author_login FROM contributions WHERE provider = ? AND host = ? AND repo = ? AND subject_id = ?
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(s.provider.as_str())
        .bind(&s.host)
        .bind(&s.repo)
        .bind(&s.subject_id)
        .fetch_optional(&self.pool)
        .await?
        .flatten())
    }

    async fn insert_contribution(&self, ev: &ForgeEvent, c: &Contributor, author: &str, status: ContributionStatus, points: i32, score: Option<f32>) -> Result<()> {
        let s = ev.subject();
        let merged_at = matches!(status, ContributionStatus::Merged).then(Utc::now);
        sqlx::query(
            "INSERT INTO contributions (id, contributor_id, github_pr_number, title, description, merit_points, status,
             created_at, merged_at, provider, host, repo, subject_id, author_login, rubric_score)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::new_v4())
        .bind(c.id)
        .bind(ev.subject_number as i32)
        .bind(&ev.title)
        .bind(&ev.body)
        .bind(points)
        .bind(status as i32)
        .bind(Utc::now())
        .bind(merged_at)
        .bind(s.as_ref().map(|s| s.provider.as_str()))
        .bind(s.as_ref().map(|s| s.host.clone()))
        .bind(s.as_ref().map(|s| s.repo.clone()))
        .bind(s.as_ref().map(|s| s.subject_id.clone()))
        .bind(author)
        .bind(score.map(|v| v as f64))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark the contribution merged: by subject; for GitHub, fall back to legacy rows
    /// (no subject columns) by PR number; insert a row when none exists.
    async fn mark_merged(&self, ev: &ForgeEvent, c: &Contributor, author: &str, points: i32, score: Option<f32>) -> Result<()> {
        let merged = ContributionStatus::Merged as i32;
        let now = Utc::now();
        let mut updated = 0;
        if let Some(s) = ev.subject() {
            updated = sqlx::query(
                "UPDATE contributions SET status = ?, merged_at = ?, merit_points = ?, rubric_score = ?,
                 author_login = COALESCE(author_login, ?)
                 WHERE provider = ? AND host = ? AND repo = ? AND subject_id = ?",
            )
            .bind(merged)
            .bind(now)
            .bind(points)
            .bind(score.map(|v| v as f64))
            .bind(author)
            .bind(s.provider.as_str())
            .bind(&s.host)
            .bind(&s.repo)
            .bind(&s.subject_id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        }
        if updated == 0 && ev.provider == ForgeProvider::GitHub {
            updated = sqlx::query(
                "UPDATE contributions SET status = ?, merged_at = ?, merit_points = ?, rubric_score = ?
                 WHERE github_pr_number = ? AND subject_id IS NULL",
            )
            .bind(merged)
            .bind(now)
            .bind(points)
            .bind(score.map(|v| v as f64))
            .bind(ev.subject_number as i32)
            .execute(&self.pool)
            .await?
            .rows_affected();
        }
        if updated == 0 {
            self.insert_contribution(ev, c, author, ContributionStatus::Merged, points, score).await?;
        }
        Ok(())
    }

    /// Merit recorded for an already-processed merge (for the payout path on redelivery).
    async fn recorded_merit(&self, ev: &ForgeEvent) -> Result<i32> {
        if let Some(s) = ev.subject() {
            let p: Option<i64> = sqlx::query_scalar(
                "SELECT merit_points FROM contributions WHERE provider = ? AND host = ? AND repo = ? AND subject_id = ?
                 ORDER BY created_at DESC LIMIT 1",
            )
            .bind(s.provider.as_str())
            .bind(&s.host)
            .bind(&s.repo)
            .bind(&s.subject_id)
            .fetch_optional(&self.pool)
            .await?;
            if let Some(p) = p {
                return Ok(p as i32);
            }
        }
        if ev.provider != ForgeProvider::GitHub {
            return Ok(0);
        }
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT merit_points FROM contributions WHERE github_pr_number = ? AND subject_id IS NULL ORDER BY created_at DESC LIMIT 1",
        )
        .bind(ev.subject_number as i32)
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or(0) as i32)
    }

    async fn record_payout(&self, ev: &ForgeEvent, username: &str, merit_points: i32) -> Result<()> {
        // Payment ledger (pending_approval only; no money moves here; idempotent on pr_id+merge_sha)
        if let Some(cfg) = &self.payments {
            match (ev.merge_sha.clone(), ev.base_branch.clone()) {
                (Some(merge_sha), Some(branch)) if ev.subject_id != 0 => {
                    let le = ledger::MergeEvent {
                        pr_id: ev.subject_id,
                        pr_number: ev.subject_number,
                        merge_sha,
                        repo: ev.repo.clone(),
                        title: ev.title.clone(),
                        github_username: username.to_string(),
                        branch,
                        merit_points: merit_points as i64,
                    };
                    ledger::record_merge(&self.pool, cfg, &le).await?;
                }
                _ => warn!("PR #{} merged without id/merge_commit_sha/base; no ledger entry", ev.subject_number),
            }
        }
        Ok(())
    }

    async fn record_activity(&self, ev: &ForgeEvent, contributor_id: Uuid, kind: ActivityType, points: i32, description: String) -> Result<()> {
        let r: Option<RepoRef> = ev.repo_ref();
        sqlx::query(
            "INSERT INTO merit_activities (id, contributor_id, activity_type, merit_change, description, metadata,
             created_at, provider, host, repo) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::new_v4())
        .bind(contributor_id)
        .bind(kind as i32)
        .bind(points)
        .bind(description)
        .bind(Option::<String>::None)
        .bind(Utc::now())
        .bind(r.as_ref().map(|r| r.provider.clone()))
        .bind(r.as_ref().map(|r| r.host.clone()))
        .bind(r.as_ref().map(|r| r.repo.clone()))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn update_contributor_points(&self, contributor_id: Uuid, points: i32) -> Result<()> {
        sqlx::query(
            "UPDATE contributors SET total_merit_points = total_merit_points + ?,
             contributions_count = contributions_count + 1, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(points)
        .bind(contributor_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
