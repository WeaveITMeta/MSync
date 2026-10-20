use crate::github::dedupe;
use crate::payments::{ledger, PaymentConfig};
use std::sync::Arc;
use crate::models::{ActivityType, Contribution, ContributionStatus, Contributor, MeritActivity};
use crate::services::coderabbit::CodeRabbitService;
use anyhow::Result;
use chrono::Utc;
use octocrab::models::webhook_events::{WebhookEvent, WebhookEventPayload};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tracing::{error, info, warn};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubWebhookPayload {
    pub action: Option<String>,
    pub pull_request: Option<PullRequestPayload>,
    pub review: Option<ReviewPayload>,
    #[serde(default)]
    pub repository: Option<RepositoryPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryPayload {
    pub full_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchRef {
    #[serde(rename = "ref")]
    pub ref_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequestPayload {
    pub number: i32,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub merged: bool,
    pub user: GitHubUser,
    /// GitHub's global PR id (stable across repos)
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub merge_commit_sha: Option<String>,
    #[serde(default)]
    pub base: Option<BranchRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewPayload {
    pub state: String,
    pub body: Option<String>,
    pub user: GitHubUser,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubUser {
    pub login: String,
}

/// Stable identity for a PR: GitHub's global id when present, else the number.
fn pr_key(pr: &PullRequestPayload) -> String {
    if pr.id != 0 { format!("id{}", pr.id) } else { format!("n{}", pr.number) }
}

pub struct GitHubWebhookHandler {
    db_pool: SqlitePool,
    coderabbit_service: CodeRabbitService,
    payments: Option<Arc<PaymentConfig>>,
}

impl GitHubWebhookHandler {
    pub fn new(db_pool: SqlitePool, coderabbit_api_key: String) -> Self {
        Self {
            db_pool,
            coderabbit_service: CodeRabbitService::new(coderabbit_api_key),
            payments: None,
        }
    }

    pub fn with_payments(mut self, cfg: Arc<PaymentConfig>) -> Self {
        self.payments = Some(cfg);
        self
    }

    pub async fn handle_webhook(&self, event_type: &str, payload: GitHubWebhookPayload) -> Result<()> {
        match event_type {
            "pull_request" => self.handle_pull_request(payload).await?,
            "pull_request_review" => self.handle_pull_request_review(payload).await?,
            _ => {
                info!("Unhandled webhook event type: {}", event_type);
            }
        }

        Ok(())
    }

    async fn handle_pull_request(&self, payload: GitHubWebhookPayload) -> Result<()> {
        let repo = payload.repository.as_ref().map(|r| r.full_name.clone());
        if let Some(pr) = payload.pull_request {
            let action = payload.action.as_deref().unwrap_or("");
            
            match action {
                "opened" => self.handle_pr_opened(pr).await?,
                "closed" => {
                    if pr.merged {
                        self.handle_pr_merged(pr, repo).await?;
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    async fn handle_pr_opened(&self, pr: PullRequestPayload) -> Result<()> {
        info!("Handling opened PR #{} by {}", pr.number, pr.user.login);

        let key = format!("pr_opened:{}", pr_key(&pr));
        if !dedupe::claim(&self.db_pool, &key).await? {
            info!("PR #{} open already processed; skipping", pr.number);
            return Ok(());
        }
        let result = self.apply_pr_opened(&pr).await;
        if result.is_err() {
            let _ = dedupe::release(&self.db_pool, &key).await;
        }
        result
    }

    async fn apply_pr_opened(&self, pr: &PullRequestPayload) -> Result<()> {

        // Get or create contributor
        let contributor = self.get_or_create_contributor(&pr.user.login).await?;

        // Create contribution record
        let contribution = Contribution::new(
            contributor.id,
            pr.number,
            pr.title.clone(),
            pr.body.clone(),
        );

        let status_i = contribution.status as i32;
        sqlx::query!(
            "INSERT INTO contributions (id, contributor_id, github_pr_number, title, description, 
             merit_points, status, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            contribution.id,
            contribution.contributor_id,
            contribution.github_pr_number,
            contribution.title,
            contribution.description,
            contribution.merit_points,
            status_i,
            contribution.created_at
        )
        .execute(&self.db_pool)
        .await?;

        // Request CodeRabbit review
        if let Err(e) = self.coderabbit_service.request_review(pr.number).await {
            error!("Failed to request CodeRabbit review: {}", e);
        }

        // Record activity
        let activity = MeritActivity::new(
            contributor.id,
            ActivityType::PullRequestOpened,
            5, // Base merit for opening PR
            format!("Opened PR #{}: {}", pr.number, pr.title),
        );

        self.record_activity(activity).await?;
        self.update_contributor_points(contributor.id, 5).await?;

        Ok(())
    }

    async fn handle_pr_merged(&self, pr: PullRequestPayload, repo: Option<String>) -> Result<()> {
        info!("Handling merged PR #{} by {}", pr.number, pr.user.login);

        let key = format!("pr_merged:{}:{}", pr_key(&pr), pr.merge_commit_sha.as_deref().unwrap_or(""));
        let merit_points = if dedupe::claim(&self.db_pool, &key).await? {
            match self.apply_pr_merged_merit(&pr).await {
                Ok(points) => points,
                Err(e) => {
                    let _ = dedupe::release(&self.db_pool, &key).await;
                    return Err(e);
                }
            }
        } else {
            info!("PR #{} merge already processed; merit not re-awarded", pr.number);
            sqlx::query_scalar::<_, i64>("SELECT merit_points FROM contributions WHERE github_pr_number = ? ORDER BY created_at DESC LIMIT 1")
                .bind(pr.number)
                .fetch_optional(&self.db_pool)
                .await?
                .unwrap_or(0) as i32
        };

        self.record_payout(&pr, repo, merit_points).await
    }

    async fn apply_pr_merged_merit(&self, pr: &PullRequestPayload) -> Result<i32> {

        // Get contributor
        let contributor = self.get_or_create_contributor(&pr.user.login).await?;

        // Update contribution status
        let merged_status = ContributionStatus::Merged as i32;
        let merged_at = Utc::now();
        sqlx::query!(
            "UPDATE contributions SET status = ?, merged_at = ? WHERE github_pr_number = ?",
            merged_status,
            merged_at,
            pr.number
        )
        .execute(&self.db_pool)
        .await?;

        // Get CodeRabbit score if available
        let coderabbit_score = self.coderabbit_service.get_review_score(pr.number).await.ok();
        
        // Calculate merit points based on CodeRabbit score
        let merit_points = self.calculate_merit_points(coderabbit_score);

        // Update contribution with merit points
        sqlx::query!(
            "UPDATE contributions SET merit_points = ?, coderabbit_score = ? WHERE github_pr_number = ?",
            merit_points,
            coderabbit_score,
            pr.number
        )
        .execute(&self.db_pool)
        .await?;

        // Record activity
        let activity = MeritActivity::new(
            contributor.id,
            ActivityType::PullRequestMerged,
            merit_points,
            format!("Merged PR #{}: {} (Score: {:?})", pr.number, pr.title, coderabbit_score),
        );

        self.record_activity(activity).await?;
        self.update_contributor_points(contributor.id, merit_points).await?;

        Ok(merit_points)
    }

    async fn record_payout(&self, pr: &PullRequestPayload, repo: Option<String>, merit_points: i32) -> Result<()> {
        // Payment ledger (pending_approval only; no money moves here; idempotent on pr_id+merge_sha)
        if let Some(cfg) = &self.payments {
            match (pr.merge_commit_sha.clone(), pr.base.as_ref().map(|b| b.ref_name.clone())) {
                (Some(merge_sha), Some(branch)) if pr.id != 0 => {
                    let ev = ledger::MergeEvent {
                        pr_id: pr.id,
                        pr_number: pr.number as i64,
                        merge_sha,
                        repo,
                        title: pr.title.clone(),
                        github_username: pr.user.login.clone(),
                        branch,
                        merit_points: merit_points as i64,
                    };
                    ledger::record_merge(&self.db_pool, cfg, &ev).await?;
                }
                _ => warn!("PR #{} merged without id/merge_commit_sha/base; no ledger entry", pr.number),
            }
        }

        Ok(())
    }

    async fn handle_pull_request_review(&self, payload: GitHubWebhookPayload) -> Result<()> {
        if let Some(review) = payload.review {
            if review.state == "approved" {
                // One approval award per reviewer per PR, however many times it's delivered
                let pr_part = payload.pull_request.as_ref().map(pr_key).unwrap_or_else(|| "unknown".into());
                let key = format!("review_approved:{}:{}", pr_part, review.user.login);
                if !dedupe::claim(&self.db_pool, &key).await? {
                    info!("Review approval by {} already awarded; skipping", review.user.login);
                    return Ok(());
                }
                // Award merit points for code reviews
                let reviewer = self.get_or_create_contributor(&review.user.login).await?;
                
                let activity = MeritActivity::new(
                    reviewer.id,
                    ActivityType::CodeReview,
                    10,
                    "Approved a pull request review".to_string(),
                );

                self.record_activity(activity).await?;
                self.update_contributor_points(reviewer.id, 10).await?;
            }
        }

        Ok(())
    }

    async fn get_or_create_contributor(&self, github_username: &str) -> Result<Contributor> {
        let existing = sqlx::query_as::<_, Contributor>(
            "SELECT * FROM contributors WHERE github_username = ?"
        )
        .bind(github_username)
        .fetch_optional(&self.db_pool)
        .await?;

        match existing {
            Some(contributor) => Ok(contributor),
            None => {
                let contributor = Contributor::new(github_username.to_string());
                
                sqlx::query!(
                    "INSERT INTO contributors (id, github_username, total_merit_points, current_role, 
                     contributions_count, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
                    contributor.id,
                    contributor.github_username,
                    contributor.total_merit_points,
                    contributor.current_role,
                    contributor.contributions_count,
                    contributor.created_at,
                    contributor.updated_at
                )
                .execute(&self.db_pool)
                .await?;

                Ok(contributor)
            }
        }
    }

    async fn record_activity(&self, activity: MeritActivity) -> Result<()> {
        let activity_type_i = activity.activity_type as i32;
        sqlx::query!(
            "INSERT INTO merit_activities (id, contributor_id, activity_type, merit_change, 
             description, metadata, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            activity.id,
            activity.contributor_id,
            activity_type_i,
            activity.merit_change,
            activity.description,
            activity.metadata,
            activity.created_at
        )
        .execute(&self.db_pool)
        .await?;

        Ok(())
    }

    async fn update_contributor_points(&self, contributor_id: Uuid, points: i32) -> Result<()> {
        sqlx::query!(
            "UPDATE contributors SET total_merit_points = total_merit_points + ?, 
             contributions_count = contributions_count + 1, updated_at = CURRENT_TIMESTAMP 
             WHERE id = ?",
            points,
            contributor_id
        )
        .execute(&self.db_pool)
        .await?;

        Ok(())
    }

    fn calculate_merit_points(&self, coderabbit_score: Option<f32>) -> i32 {
        match coderabbit_score {
            Some(score) if score >= 90.0 => 50, // Excellent
            Some(score) if score >= 80.0 => 35, // Good
            Some(score) if score >= 70.0 => 25, // Average
            Some(score) if score >= 60.0 => 15, // Below average
            _ => 10, // Base points for merged PR
        }
    }
}

#[cfg(test)]
mod dedupe_tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn pr_payload(action: &str, merged: bool) -> GitHubWebhookPayload {
        serde_json::from_value(serde_json::json!({
            "action": action,
            "repository": {"full_name": "org/eustress"},
            "pull_request": {
                "id": 9001, "number": 7, "title": "Add thing", "body": null,
                "state": if merged { "closed" } else { "open" }, "merged": merged,
                "user": {"login": "alice"},
                "merge_commit_sha": if merged { Some("abc123") } else { None },
                "base": {"ref": "main"}
            }
        }))
        .unwrap()
    }

    async fn points(pool: &SqlitePool) -> (i64, i64, i64) {
        let p: i64 = sqlx::query_scalar("SELECT total_merit_points FROM contributors WHERE github_username = 'alice'")
            .fetch_one(pool).await.unwrap();
        let acts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM merit_activities").fetch_one(pool).await.unwrap();
        let contribs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM contributions").fetch_one(pool).await.unwrap();
        (p, acts, contribs)
    }

    #[tokio::test]
    async fn duplicate_deliveries_do_not_double_award_merit() {
        let pool = pool().await;
        let cfg = Arc::new(PaymentConfig::from_lookup(|_| None));
        let h = GitHubWebhookHandler::new(pool.clone(), "k".into()).with_payments(cfg);

        h.handle_webhook("pull_request", pr_payload("opened", false)).await.unwrap();
        let after_open = points(&pool).await;
        h.handle_webhook("pull_request", pr_payload("opened", false)).await.unwrap();
        assert_eq!(points(&pool).await, after_open, "duplicate open changed state");

        h.handle_webhook("pull_request", pr_payload("closed", true)).await.unwrap();
        let after_merge = points(&pool).await;
        assert!(after_merge.0 > after_open.0);
        h.handle_webhook("pull_request", pr_payload("closed", true)).await.unwrap();
        assert_eq!(points(&pool).await, after_merge, "duplicate merge changed state");

        let ledger_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payout_ledger").fetch_one(&pool).await.unwrap();
        assert_eq!(ledger_rows, 1);
    }

    #[tokio::test]
    async fn duplicate_review_approval_awards_once() {
        let pool = pool().await;
        let h = GitHubWebhookHandler::new(pool.clone(), "k".into());
        let mut payload = pr_payload("submitted", false);
        payload.review = Some(ReviewPayload { state: "approved".into(), body: None, user: GitHubUser { login: "alice".into() } });
        h.handle_webhook("pull_request_review", payload.clone()).await.unwrap();
        let first = points(&pool).await;
        h.handle_webhook("pull_request_review", payload).await.unwrap();
        assert_eq!(points(&pool).await, first);
    }

    #[tokio::test]
    async fn claim_and_release() {
        let pool = pool().await;
        assert!(dedupe::claim(&pool, "delivery:1").await.unwrap());
        assert!(!dedupe::claim(&pool, "delivery:1").await.unwrap());
        dedupe::release(&pool, "delivery:1").await.unwrap();
        assert!(dedupe::claim(&pool, "delivery:1").await.unwrap());
    }
}
