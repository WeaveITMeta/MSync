use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Contributor {
    pub id: Uuid,
    pub github_username: String,
    pub discord_user_id: Option<i64>,
    pub total_merit_points: i32,
    pub current_role: String,
    pub contributions_count: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Contribution {
    pub id: Uuid,
    pub contributor_id: Uuid,
    pub github_pr_number: i32,
    pub title: String,
    pub description: Option<String>,
    pub merit_points: i32,
    pub status: ContributionStatus,
    pub coderabbit_score: Option<f32>,
    pub created_at: DateTime<Utc>,
    pub merged_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
pub enum ContributionStatus {
    Pending,
    UnderReview,
    Approved,
    Merged,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct MeritRole {
    pub id: Uuid,
    pub role_name: String,
    pub discord_role_id: i64,
    pub required_merit_points: i32,
    pub permissions_level: i32,
    pub description: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct MeritActivity {
    pub id: Uuid,
    pub contributor_id: Uuid,
    pub activity_type: ActivityType,
    pub merit_change: i32,
    pub description: String,
    pub metadata: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT")]
pub enum ActivityType {
    PullRequestOpened,
    PullRequestMerged,
    CodeReview,
    IssueResolved,
    DocumentationUpdate,
    CommunityHelp,
    BugFix,
    FeatureImplementation,
    SecurityFix,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaderboardEntry {
    pub rank: i32,
    pub contributor: Contributor,
    pub recent_contributions: Vec<Contribution>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContributorStats {
    pub contributor: Contributor,
    pub total_contributions: i32,
    pub merged_contributions: i32,
    pub average_coderabbit_score: f32,
    pub recent_activities: Vec<MeritActivity>,
    pub current_streak_days: i32,
}

impl Contributor {
    pub fn new(github_username: String) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            github_username,
            discord_user_id: None,
            total_merit_points: 0,
            current_role: "Newcomer".to_string(),
            contributions_count: 0,
            created_at: now,
            updated_at: now,
        }
    }
}

impl Contribution {
    pub fn new(
        contributor_id: Uuid,
        github_pr_number: i32,
        title: String,
        description: Option<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            contributor_id,
            github_pr_number,
            title,
            description,
            merit_points: 0,
            status: ContributionStatus::Pending,
            coderabbit_score: None,
            created_at: Utc::now(),
            merged_at: None,
        }
    }
}

impl MeritActivity {
    pub fn new(
        contributor_id: Uuid,
        activity_type: ActivityType,
        merit_change: i32,
        description: String,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            contributor_id,
            activity_type,
            merit_change,
            description,
            metadata: None,
            created_at: Utc::now(),
        }
    }
}
