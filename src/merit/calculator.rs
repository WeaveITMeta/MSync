//! Bonus helpers. Event points come from ONE function, `merit::rubric::points`, with
//! the per-event base/max in `merit::scoring::MeritRules`; the old per-activity table
//! and the CodeRabbit score step table that used to live here were removed.

pub struct MeritCalculator;

impl MeritCalculator {
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
