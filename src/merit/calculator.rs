use crate::models::ActivityType;

pub struct MeritCalculator;

impl MeritCalculator {
    pub fn calculate_activity_merit(activity_type: &ActivityType, metadata: Option<&serde_json::Value>) -> i32 {
        match activity_type {
            ActivityType::PullRequestOpened => 5,
            ActivityType::PullRequestMerged => {
                // Base merit for merge, can be enhanced by CodeRabbit score
                if let Some(meta) = metadata {
                    if let Some(score) = meta.get("coderabbit_score").and_then(|s| s.as_f64()) {
                        return Self::calculate_pr_merit_from_score(score as f32);
                    }
                }
                25
            }
            ActivityType::CodeReview => 10,
            ActivityType::IssueResolved => 15,
            ActivityType::DocumentationUpdate => 20,
            ActivityType::CommunityHelp => 5,
            ActivityType::BugFix => 30,
            ActivityType::FeatureImplementation => 40,
            ActivityType::SecurityFix => 50,
        }
    }

    pub fn calculate_pr_merit_from_score(coderabbit_score: f32) -> i32 {
        match coderabbit_score {
            score if score >= 95.0 => 60,  // Exceptional
            score if score >= 90.0 => 50,  // Excellent
            score if score >= 80.0 => 35,  // Good
            score if score >= 70.0 => 25,  // Average
            score if score >= 60.0 => 15,  // Below average
            _ => 10,                       // Base points
        }
    }

    pub fn calculate_streak_bonus(streak_days: i32) -> i32 {
        match streak_days {
            days if days >= 30 => 50,
            days if days >= 14 => 25,
            days if days >= 7 => 10,
            days if days >= 3 => 5,
            _ => 0,
        }
    }

    pub fn calculate_complexity_bonus(lines_changed: i32, files_changed: i32) -> i32 {
        let complexity_score = (lines_changed as f32 * 0.1 + files_changed as f32 * 2.0).min(20.0);
        complexity_score as i32
    }

    pub fn calculate_review_quality_bonus(review_length: usize, suggestions_count: i32) -> i32 {
        let length_bonus = match review_length {
            len if len >= 500 => 5,
            len if len >= 200 => 3,
            len if len >= 50 => 1,
            _ => 0,
        };

        let suggestion_bonus = (suggestions_count * 2).min(10);
        
        length_bonus + suggestion_bonus
    }
}
