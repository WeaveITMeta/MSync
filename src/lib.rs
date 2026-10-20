pub mod config;
pub mod discord;
pub mod github;
pub mod merit;
pub mod models;
pub mod routes;
pub mod services;

use config::Config;
use merit::tracker::MeritTracker;
use sqlx::sqlite::SqlitePool;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct AppState {
    pub config: Config,
    pub db_pool: SqlitePool,
    pub merit_tracker: Arc<RwLock<MeritTracker>>,
    pub discord_http: Arc<serenity::http::Http>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Contributor, Contribution, ActivityType, MeritActivity};
    use crate::merit::calculator::MeritCalculator;
    use uuid::Uuid;

    #[test]
    fn test_contributor_creation() {
        let contributor = Contributor::new("testuser".to_string());
        assert_eq!(contributor.github_username, "testuser");
        assert_eq!(contributor.total_merit_points, 0);
        assert_eq!(contributor.current_role, "Newcomer");
        assert_eq!(contributor.contributions_count, 0);
        assert!(contributor.discord_user_id.is_none());
    }

    #[test]
    fn test_contribution_creation() {
        let contributor_id = Uuid::new_v4();
        let contribution = Contribution::new(
            contributor_id,
            123,
            "Fix bug in authentication".to_string(),
            Some("Resolved issue #456".to_string()),
        );
        
        assert_eq!(contribution.contributor_id, contributor_id);
        assert_eq!(contribution.github_pr_number, 123);
        assert_eq!(contribution.title, "Fix bug in authentication");
        assert_eq!(contribution.merit_points, 0);
        assert!(contribution.coderabbit_score.is_none());
    }

    #[test]
    fn test_merit_calculation_for_activities() {
        
        assert_eq!(MeritCalculator::calculate_activity_merit(&ActivityType::PullRequestOpened, None), 5);
        assert_eq!(MeritCalculator::calculate_activity_merit(&ActivityType::CodeReview, None), 10);
        assert_eq!(MeritCalculator::calculate_activity_merit(&ActivityType::BugFix, None), 30);
        assert_eq!(MeritCalculator::calculate_activity_merit(&ActivityType::SecurityFix, None), 50);
    }

    #[test]
    fn test_merit_calculation_from_coderabbit_score() {
        
        assert_eq!(MeritCalculator::calculate_pr_merit_from_score(95.0), 60);
        assert_eq!(MeritCalculator::calculate_pr_merit_from_score(90.0), 50);
        assert_eq!(MeritCalculator::calculate_pr_merit_from_score(80.0), 35);
        assert_eq!(MeritCalculator::calculate_pr_merit_from_score(70.0), 25);
        assert_eq!(MeritCalculator::calculate_pr_merit_from_score(50.0), 10);
    }

    #[test]
    fn test_streak_bonus_calculation() {
        
        assert_eq!(MeritCalculator::calculate_streak_bonus(1), 0);
        assert_eq!(MeritCalculator::calculate_streak_bonus(3), 5);
        assert_eq!(MeritCalculator::calculate_streak_bonus(7), 10);
        assert_eq!(MeritCalculator::calculate_streak_bonus(14), 25);
        assert_eq!(MeritCalculator::calculate_streak_bonus(30), 50);
    }

    #[test]
    fn test_complexity_bonus() {
        
        // 50 lines * 0.1 + 2 files * 2.0 = 9
        assert_eq!(MeritCalculator::calculate_complexity_bonus(50, 2), 9);
        assert_eq!(MeritCalculator::calculate_complexity_bonus(100, 5), 20);
        assert_eq!(MeritCalculator::calculate_complexity_bonus(500, 10), 20); // Capped at 20
    }

    #[test]
    fn test_config_validation() {
        use std::env;
        
        // Test that config properly validates required fields
        env::set_var("DISCORD_TOKEN", "test_token");
        env::set_var("DISCORD_APPLICATION_ID", "123456");
        env::set_var("GITHUB_TOKEN", "gh_test");
        env::set_var("GITHUB_WEBHOOK_SECRET", "secret");
        env::set_var("GITHUB_REPO_OWNER", "owner");
        env::set_var("GITHUB_REPO_NAME", "repo");
        env::set_var("CODERABBIT_API_KEY", "cr_test");
        
        let config = config::Config::from_env();
        assert!(config.is_ok());
        
        let config = config.unwrap();
        assert_eq!(config.discord.token, "test_token");
        assert_eq!(config.github.repo_owner, "owner");
    }
}
