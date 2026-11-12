//! `dsync-rubric-v1`: the fixed merit rubric (contract section 6).
//!
//! The rubric is a constant. Its canonical JSON is pinned by SHA-256 in
//! [`RUBRIC_SHA256`]; changing a weight or an anchor changes the hash and fails
//! `rubric_hash_is_pinned`. A change means publishing `dsync-rubric-v2`, never
//! editing v1.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const RUBRIC_ID: &str = "dsync-rubric-v1";

/// SHA-256 (lowercase hex) of [`canonical_json`]. Pinned; see the module docs.
pub const RUBRIC_SHA256: &str = "b0e7b801aaf29a4c7b2cb4ac216673040af893bc538309dbc565f2a1bc4faa78";

pub const MIN_LEVEL: u8 = 0;
pub const MAX_LEVEL: u8 = 4;
pub const MIN_EVIDENCE_CHARS: usize = 20;
/// `ci_passed = false` caps the score here.
pub const CI_FAILED_CAP: f32 = 40.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Criterion {
    pub key: &'static str,
    pub weight: u32,
    /// Anchor text for levels 0..=4.
    pub levels: [&'static str; 5],
}

pub const CRITERIA: [Criterion; 6] = [
    Criterion {
        key: "impact",
        weight: 30,
        levels: [
            "no user or project effect",
            "cosmetic",
            "useful local improvement",
            "unblocks others or fixes a real defect",
            "moves a roadmap goal or fixes a critical defect",
        ],
    },
    Criterion {
        key: "correctness",
        weight: 20,
        levels: [
            "broken or untested",
            "works by inspection only",
            "tested on the main path",
            "tests cover edge cases",
            "tests would catch a regression and were shown to fail without the change",
        ],
    },
    Criterion {
        key: "difficulty",
        weight: 15,
        levels: [
            "trivial",
            "mechanical",
            "needs module knowledge",
            "crosses modules or needs design",
            "hard problem, novel design or deep debugging",
        ],
    },
    Criterion {
        key: "quality",
        weight: 15,
        levels: [
            "hurts maintainability",
            "works but unclear",
            "clear and consistent",
            "simplifies what it touches",
            "leaves the area measurably better",
        ],
    },
    Criterion {
        key: "risk",
        weight: 10,
        levels: [
            "adds a security or data-loss risk",
            "unexamined risk",
            "no new risk",
            "risk identified and contained",
            "removes an existing risk",
        ],
    },
    Criterion {
        key: "communication",
        weight: 10,
        levels: [
            "no description",
            "states what",
            "states what and why",
            "reviewer can verify from the description",
            "docs and changelog updated where needed",
        ],
    },
];

fn json_str(s: &str) -> String {
    serde_json::to_string(s).expect("string serialization cannot fail")
}

/// Canonical JSON of the rubric. Built by hand (fixed key order, no whitespace) so
/// the bytes never depend on serde_json feature flags.
pub fn canonical_json() -> String {
    let mut out = String::new();
    out.push_str("{\"id\":");
    out.push_str(&json_str(RUBRIC_ID));
    out.push_str(",\"criteria\":[");
    for (i, c) in CRITERIA.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"key\":");
        out.push_str(&json_str(c.key));
        out.push_str(&format!(",\"weight\":{}", c.weight));
        out.push_str(",\"levels\":[");
        for (j, l) in c.levels.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str(&json_str(l));
        }
        out.push_str("]}");
    }
    out.push_str("],\"score\":\"sum(weight * level / 4), 0..100, one decimal\"");
    out.push_str(&format!(
        ",\"gates\":{{\"ci_failed_cap\":{},\"reverted\":0}}",
        CI_FAILED_CAP as u32
    ));
    out.push_str(&format!(",\"min_evidence_chars\":{}", MIN_EVIDENCE_CHARS));
    out.push_str(",\"points\":\"base_points + round((max_points - base_points) * score / 100)\"}");
    out
}

/// SHA-256 of [`canonical_json`], computed now. Must equal [`RUBRIC_SHA256`].
pub fn computed_hash() -> String {
    hex::encode(Sha256::digest(canonical_json().as_bytes()))
}

/// One criterion's level with its evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CriterionScore {
    pub criterion: String,
    pub level: u8,
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RubricError {
    #[error("unknown rubric id '{0}' (this engine knows {RUBRIC_ID})")]
    UnknownRubric(String),
    #[error("rubric hash does not match {RUBRIC_ID}")]
    HashMismatch,
    #[error("unknown criterion '{0}'")]
    UnknownCriterion(String),
    #[error("criterion '{0}' given more than once")]
    DuplicateCriterion(String),
    #[error("criterion '{0}' missing")]
    MissingCriterion(String),
    #[error("criterion '{0}' level {1} is outside 0..=4")]
    LevelOutOfRange(String, u8),
    #[error("evidence for '{0}' is shorter than {MIN_EVIDENCE_CHARS} characters")]
    EvidenceTooShort(String),
    #[error("total score {0} is outside 0..=100")]
    TotalOutOfRange(f32),
}

fn round1(x: f64) -> f32 {
    ((x * 10.0).round() / 10.0) as f32
}

fn evidence_ok(e: &str) -> bool {
    e.trim().chars().count() >= MIN_EVIDENCE_CHARS
}

/// The submission must echo this rubric's id and hash.
pub fn validate_header(rubric_id: &str, rubric_hash: &str) -> Result<(), RubricError> {
    if rubric_id != RUBRIC_ID {
        return Err(RubricError::UnknownRubric(rubric_id.to_string()));
    }
    if !rubric_hash.trim().eq_ignore_ascii_case(RUBRIC_SHA256) {
        return Err(RubricError::HashMismatch);
    }
    Ok(())
}

/// Validate per-criterion levels (all six, once each, 0..=4, evidence >= 20 chars)
/// and return the raw score `sum(weight * level / 4)` rounded to one decimal.
pub fn score_levels(levels: &[CriterionScore]) -> Result<f32, RubricError> {
    let mut seen = [false; CRITERIA.len()];
    let mut sum = 0.0f64;
    for cs in levels {
        let Some(idx) = CRITERIA.iter().position(|c| c.key == cs.criterion) else {
            return Err(RubricError::UnknownCriterion(cs.criterion.clone()));
        };
        if seen[idx] {
            return Err(RubricError::DuplicateCriterion(cs.criterion.clone()));
        }
        seen[idx] = true;
        if cs.level > MAX_LEVEL {
            return Err(RubricError::LevelOutOfRange(cs.criterion.clone(), cs.level));
        }
        if !evidence_ok(&cs.evidence) {
            return Err(RubricError::EvidenceTooShort(cs.criterion.clone()));
        }
        sum += CRITERIA[idx].weight as f64 * cs.level as f64 / MAX_LEVEL as f64;
    }
    if let Some(i) = seen.iter().position(|s| !s) {
        return Err(RubricError::MissingCriterion(CRITERIA[i].key.to_string()));
    }
    Ok(round1(sum))
}

/// Validate a total score (CodeRabbit-style submission) with its single evidence string.
pub fn validate_total(total: f32, evidence: &str) -> Result<f32, RubricError> {
    if !total.is_finite() || !(0.0..=100.0).contains(&total) {
        return Err(RubricError::TotalOutOfRange(total));
    }
    if !evidence_ok(evidence) {
        return Err(RubricError::EvidenceTooShort("total".into()));
    }
    Ok(round1(total as f64))
}

/// Hard gates: `reverted` sets 0; `ci_passed = false` caps at 40.
pub fn apply_gates(raw: f32, ci_passed: bool, reverted: bool) -> f32 {
    if reverted {
        return 0.0;
    }
    let s = raw.clamp(0.0, 100.0);
    if !ci_passed {
        s.min(CI_FAILED_CAP)
    } else {
        s
    }
}

/// Full rubric score from validated levels plus gates.
pub fn score(levels: &[CriterionScore], ci_passed: bool, reverted: bool) -> Result<f32, RubricError> {
    Ok(apply_gates(score_levels(levels)?, ci_passed, reverted))
}

/// THE points function (one function, one table):
/// `points = base + round((max - base) * score / 100)`. `None` (no score) earns `base`.
pub fn points(base_points: i32, max_points: i32, score: Option<f32>) -> i32 {
    match score {
        None => base_points,
        Some(s) => {
            let s = if s.is_finite() { s.clamp(0.0, 100.0) } else { 0.0 } as f64;
            base_points + ((max_points - base_points) as f64 * s / 100.0).round() as i32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(level: u8) -> Vec<CriterionScore> {
        CRITERIA
            .iter()
            .map(|c| CriterionScore {
                criterion: c.key.into(),
                level,
                evidence: "evidence that is long enough to count".into(),
            })
            .collect()
    }

    #[test]
    fn rubric_hash_is_pinned() {
        assert_eq!(computed_hash(), RUBRIC_SHA256, "rubric v1 changed; publish v2 instead");
    }

    #[test]
    fn weights_and_ids_match_contract() {
        let w: Vec<(&str, u32)> = CRITERIA.iter().map(|c| (c.key, c.weight)).collect();
        assert_eq!(
            w,
            vec![("impact", 30), ("correctness", 20), ("difficulty", 15), ("quality", 15), ("risk", 10), ("communication", 10)]
        );
        assert_eq!(CRITERIA.iter().map(|c| c.weight).sum::<u32>(), 100);
        assert_eq!(RUBRIC_ID, "dsync-rubric-v1");
    }

    #[test]
    fn score_formula() {
        assert_eq!(score(&all(4), true, false).unwrap(), 100.0);
        assert_eq!(score(&all(0), true, false).unwrap(), 0.0);
        assert_eq!(score(&all(2), true, false).unwrap(), 50.0);
        // impact 3 (22.5) + correctness 1 (5) + difficulty 1 (3.75) + others 0 = 31.25 -> 31.3
        let mut l = all(0);
        l[0].level = 3;
        l[1].level = 1;
        l[2].level = 1;
        assert_eq!(score(&l, true, false).unwrap(), 31.3);
    }

    #[test]
    fn ci_failed_caps_at_40() {
        assert_eq!(score(&all(4), false, false).unwrap(), 40.0);
        assert_eq!(score(&all(1), false, false).unwrap(), 25.0); // below cap unchanged
    }

    #[test]
    fn reverted_scores_zero() {
        assert_eq!(score(&all(4), true, true).unwrap(), 0.0);
        assert_eq!(score(&all(4), false, true).unwrap(), 0.0);
    }

    #[test]
    fn validation_rejects_bad_submissions() {
        let mut l = all(2);
        l[0].level = 5;
        assert!(matches!(score_levels(&l), Err(RubricError::LevelOutOfRange(..))));
        let mut l = all(2);
        l[3].evidence = "too short".into();
        assert!(matches!(score_levels(&l), Err(RubricError::EvidenceTooShort(..))));
        let mut l = all(2);
        l[3].evidence = "                         x".into(); // whitespace does not count
        assert!(matches!(score_levels(&l), Err(RubricError::EvidenceTooShort(..))));
        let mut l = all(2);
        l.pop();
        assert!(matches!(score_levels(&l), Err(RubricError::MissingCriterion(..))));
        let mut l = all(2);
        l[5].criterion = "impact".into();
        assert!(matches!(score_levels(&l), Err(RubricError::DuplicateCriterion(..))));
        let mut l = all(2);
        l[5].criterion = "vibes".into();
        assert!(matches!(score_levels(&l), Err(RubricError::UnknownCriterion(..))));
        assert!(validate_header(RUBRIC_ID, RUBRIC_SHA256).is_ok());
        assert_eq!(validate_header("dsync-rubric-v2", RUBRIC_SHA256), Err(RubricError::UnknownRubric("dsync-rubric-v2".into())));
        assert_eq!(validate_header(RUBRIC_ID, "00"), Err(RubricError::HashMismatch));
        assert!(validate_total(101.0, "evidence that is long enough").is_err());
        assert!(validate_total(f32::NAN, "evidence that is long enough").is_err());
        assert!(validate_total(80.0, "short").is_err());
        assert_eq!(validate_total(80.04, "evidence that is long enough").unwrap(), 80.0);
    }

    #[test]
    fn points_function() {
        assert_eq!(points(10, 60, None), 10);
        assert_eq!(points(10, 60, Some(0.0)), 10);
        assert_eq!(points(10, 60, Some(100.0)), 60);
        assert_eq!(points(10, 60, Some(50.0)), 35);
        assert_eq!(points(10, 60, Some(31.3)), 26); // 15.65 -> 16
        assert_eq!(points(10, 60, Some(40.0)), 30);
        assert_eq!(points(5, 5, Some(100.0)), 5);
    }
}
