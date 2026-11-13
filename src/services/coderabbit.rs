//! CodeRabbit as an optional scorer source (contract section 6).
//!
//! There is no CodeRabbit network call and no score here. The engine never invents
//! a score: a CodeRabbit result reaches merit only as a `Submission` with
//! `Assessment::Total` (0..=100 plus one evidence string), stored through
//! `merit::scoring::submit_score` like any other scorer.

use crate::foundry::{FoundryProvider, Subject};
use crate::merit::rubric::{RUBRIC_ID, RUBRIC_SHA256};
use crate::merit::scoring::{Assessment, Scorer, ScorerSource, Submission};

/// Scorer id used for CodeRabbit submissions.
pub const CODERABBIT_SCORER_ID: &str = "coderabbit";

/// The CodeRabbit scorer. Its bot logins are listed so it can never score work it
/// authored (for example a PR opened by the bot itself).
pub fn coderabbit_scorer() -> Scorer {
    Scorer {
        id: CODERABBIT_SCORER_ID.to_string(),
        source: ScorerSource::CodeRabbit,
        foundry_logins: vec![
            (FoundryProvider::GitHub, "coderabbitai".to_string()),
            (FoundryProvider::GitHub, "coderabbitai[bot]".to_string()),
            (FoundryProvider::GitLab, "coderabbitai".to_string()),
        ],
    }
}

/// Build a CodeRabbit submission from a real review result. Validation happens in
/// `submit_score`.
pub fn coderabbit_submission(subject: Subject, total_score: f32, evidence: String, ci_passed: bool, reverted: bool) -> Submission {
    Submission {
        subject,
        rubric_id: RUBRIC_ID.to_string(),
        rubric_hash: RUBRIC_SHA256.to_string(),
        ci_passed,
        reverted,
        assessment: Assessment::Total { score: total_score, evidence },
    }
}
