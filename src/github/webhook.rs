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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequestPayload {
    pub number: i32,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub merged: bool,
    pub user: GitHubUser,
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

pub struct GitHubWebhookHandler {
    db_pool: SqlitePool,
    coderabbit_service: CodeRabbitService,
}

impl GitHubWebhookHandler {
    pub fn new(db_pool: SqlitePool, coderabbit_api_key: String) -> Self {
        Self {
            db_pool,
            coderabbit_service: CodeRabbitService::new(coderabbit_api_key),
        }
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
        if let Some(pr) = payload.pull_request {
            let action = payload.action.as_deref().unwrap_or("");
            
            match action {
                "opened" => self.handle_pr_opened(pr).await?,
                "closed" => {
                    if pr.merged {
                        self.handle_pr_merged(pr).await?;
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    async fn handle_pr_opened(&self, pr: PullRequestPayload) -> Result<()> {
        info!("Handling opened PR #{} by {}", pr.number, pr.user.login);

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

    async fn handle_pr_merged(&self, pr: PullRequestPayload) -> Result<()> {
        info!("Handling merged PR #{} by {}", pr.number, pr.user.login);

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

        Ok(())
    }

    async fn handle_pull_request_review(&self, payload: GitHubWebhookPayload) -> Result<()> {
        if let Some(review) = payload.review {
            if review.state == "approved" {
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
