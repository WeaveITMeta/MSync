//! Rubric score submissions and the merit they adjust (contract section 6).
//!
//! Library functions only. The REST/MCP layer (a later worker) authenticates the
//! caller and builds the [`Scorer`]; nothing here trusts a request body to name its
//! own scorer.

use super::rubric::{self, CriterionScore, RubricError, RUBRIC_ID, RUBRIC_SHA256};
use crate::forge::{legacy_username, provider_key, ForgeProvider, Subject};
use crate::models::ActivityType;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScorerSource {
    Agent,
    CodeRabbit,
}

impl ScorerSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            ScorerSource::Agent => "agent",
            ScorerSource::CodeRabbit => "coderabbit",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "agent" => Some(ScorerSource::Agent),
            "coderabbit" => Some(ScorerSource::CodeRabbit),
            _ => None,
        }
    }
}

/// Who is scoring. Supplied by the caller from an authenticated token, never from the body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scorer {
    pub id: String,
    pub source: ScorerSource,
    /// The scorer's own forge logins; a scorer may not score a subject authored by any of them.
    pub forge_logins: Vec<(ForgeProvider, String)>,
}

/// What a scorer submits. Agents submit per-criterion levels; CodeRabbit submits a
/// total score 0..=100 with one evidence string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Assessment {
    Rubric { criteria: Vec<CriterionScore> },
    Total { score: f32, evidence: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Submission {
    pub subject: Subject,
    /// Must echo [`RUBRIC_ID`].
    pub rubric_id: String,
    /// Must echo [`RUBRIC_SHA256`].
    pub rubric_hash: String,
    pub ci_passed: bool,
    pub reverted: bool,
    pub assessment: Assessment,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScoreRecord {
    pub id: String,
    pub subject: Subject,
    pub scorer_id: String,
    pub scorer_source: ScorerSource,
    pub rubric_id: String,
    pub rubric_hash: String,
    /// 0..=100 after gates, one decimal.
    pub score: f32,
    pub ci_passed: bool,
    pub reverted: bool,
    pub detail: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum ScoreError {
    #[error(transparent)]
    Rubric(#[from] RubricError),
    #[error("{0}")]
    WrongShape(&'static str),
    #[error("scorer id is empty")]
    EmptyScorerId,
    #[error("unknown subject: no contribution recorded for it")]
    UnknownSubject,
    #[error("a scorer may not score work they authored")]
    SelfScore,
    #[error("this scorer already scored this subject")]
    DuplicateScorer,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Points for one event kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRule {
    pub base_points: i32,
    pub max_points: i32,
}

impl EventRule {
    pub fn points(&self, score: Option<f32>) -> i32 {
        rubric::points(self.base_points, self.max_points, score)
    }
}

/// Merit rules. `Default` is contract 3.4 until the CRA supplies `merit.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeritRules {
    pub rubric: String,
    /// Sources whose scores count. Empty = base points only.
    pub scorers: Vec<ScorerSource>,
    pub min_scorers: usize,
    pub score_window_days: i64,
    pub pr_opened: EventRule,
    pub pr_merged: EventRule,
    pub review_approved: EventRule,
}

impl Default for MeritRules {
    fn default() -> Self {
        Self {
            rubric: RUBRIC_ID.to_string(),
            scorers: vec![ScorerSource::Agent],
            min_scorers: 1,
            score_window_days: 7,
            pr_opened: EventRule { base_points: 5, max_points: 5 },
            pr_merged: EventRule { base_points: 10, max_points: 60 },
            review_approved: EventRule { base_points: 10, max_points: 10 },
        }
    }
}

/// Validate a submission for a scorer source and return its gated score.
pub fn evaluate(sub: &Submission, source: ScorerSource) -> Result<f32, ScoreError> {
    rubric::validate_header(&sub.rubric_id, &sub.rubric_hash)?;
    let raw = match (&sub.assessment, source) {
        (Assessment::Rubric { criteria }, ScorerSource::Agent) => rubric::score_levels(criteria)?,
        (Assessment::Total { score, evidence }, ScorerSource::CodeRabbit) => rubric::validate_total(*score, evidence)?,
        (Assessment::Total { .. }, ScorerSource::Agent) => {
            return Err(ScoreError::WrongShape("an agent submits per-criterion levels, not a total"))
        }
        (Assessment::Rubric { .. }, ScorerSource::CodeRabbit) => {
            return Err(ScoreError::WrongShape("a coderabbit submission carries a total score"))
        }
    };
    Ok(rubric::apply_gates(raw, sub.ci_passed, sub.reverted))
}

struct SubjectRow {
    id: Uuid,
    contributor_id: Uuid,
    author_login: Option<String>,
    merit_points: i64,
    status: i64,
    merged_at: Option<DateTime<Utc>>,
}

async fn load_subject(pool: &SqlitePool, s: &Subject) -> Result<Option<SubjectRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, contributor_id, author_login, merit_points, status, merged_at FROM contributions
         WHERE provider = ? AND host = ? AND repo = ? AND subject_id = ? ORDER BY created_at DESC LIMIT 1",
    )
    .bind(s.provider.as_str())
    .bind(&s.host)
    .bind(&s.repo)
    .bind(&s.subject_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    Ok(Some(SubjectRow {
        id: row.try_get("id")?,
        contributor_id: row.try_get("contributor_id")?,
        author_login: row.try_get("author_login")?,
        merit_points: row.try_get("merit_points")?,
        status: row.try_get("status")?,
        merged_at: row.try_get("merged_at")?,
    }))
}

/// Is any of the scorer's forge logins the subject's author (directly, through a
/// linked forge identity, or through the legacy username)?
async fn is_author(pool: &SqlitePool, s: &Subject, row: &SubjectRow, scorer: &Scorer) -> Result<bool, sqlx::Error> {
    for (provider, login) in &scorer.forge_logins {
        let login_n = login.trim().to_ascii_lowercase();
        if login_n.is_empty() {
            continue;
        }
        if *provider == s.provider && row.author_login.as_deref().map(|a| a.eq_ignore_ascii_case(&login_n)).unwrap_or(false) {
            return Ok(true);
        }
        let linked: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM forge_identities WHERE provider = ? AND login = ? AND contributor_id = ?",
        )
        .bind(provider.as_str())
        .bind(&login_n)
        .bind(row.contributor_id)
        .fetch_one(pool)
        .await?;
        if linked > 0 {
            return Ok(true);
        }
        let mut hosts = vec![provider.default_host().to_string()];
        if *provider == s.provider {
            hosts.push(s.host.clone());
        }
        for h in hosts {
            let legacy: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM contributors WHERE lower(github_username) = lower(?) AND id = ?")
                .bind(legacy_username(*provider, &h, login.trim()))
                .bind(row.contributor_id)
                .fetch_one(pool)
                .await?;
            if legacy > 0 {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Store one score submission. Refuses a scorer who authored the subject, a second
/// submission by the same scorer, an unknown subject, and anything the rubric rejects.
pub async fn submit_score(pool: &SqlitePool, sub: &Submission, scorer: &Scorer) -> Result<ScoreRecord, ScoreError> {
    if scorer.id.trim().is_empty() {
        return Err(ScoreError::EmptyScorerId);
    }
    let score = evaluate(sub, scorer.source)?;
    let subject = Subject::new(sub.subject.provider, &sub.subject.host, &sub.subject.repo, &sub.subject.subject_id);
    let row = load_subject(pool, &subject).await?.ok_or(ScoreError::UnknownSubject)?;
    if is_author(pool, &subject, &row, scorer).await? {
        return Err(ScoreError::SelfScore);
    }

    let detail = serde_json::to_value(&sub.assessment).expect("assessment serializes");
    let record = ScoreRecord {
        id: Uuid::new_v4().to_string(),
        subject: subject.clone(),
        scorer_id: scorer.id.trim().to_string(),
        scorer_source: scorer.source,
        rubric_id: RUBRIC_ID.to_string(),
        rubric_hash: RUBRIC_SHA256.to_string(),
        score,
        ci_passed: sub.ci_passed,
        reverted: sub.reverted,
        detail,
        created_at: Utc::now(),
    };
    let res = sqlx::query(
        "INSERT OR IGNORE INTO merit_scores (id, provider, host, repo, subject_id, scorer_id, scorer_source,
         rubric_id, rubric_hash, score, ci_passed, reverted, detail, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&record.id)
    .bind(subject.provider.as_str())
    .bind(&subject.host)
    .bind(&subject.repo)
    .bind(&subject.subject_id)
    .bind(&record.scorer_id)
    .bind(record.scorer_source.as_str())
    .bind(&record.rubric_id)
    .bind(&record.rubric_hash)
    .bind(record.score as f64)
    .bind(record.ci_passed)
    .bind(record.reverted)
    .bind(record.detail.to_string())
    .bind(record.created_at)
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(ScoreError::DuplicateScorer);
    }
    Ok(record)
}

/// All submissions for a subject, oldest first.
pub async fn list_scores(pool: &SqlitePool, subject: &Subject) -> Result<Vec<ScoreRecord>, sqlx::Error> {
    let s = Subject::new(subject.provider, &subject.host, &subject.repo, &subject.subject_id);
    let rows = sqlx::query(
        "SELECT id, scorer_id, scorer_source, rubric_id, rubric_hash, score, ci_passed, reverted, detail, created_at
         FROM merit_scores WHERE provider = ? AND host = ? AND repo = ? AND subject_id = ?",
    )
    .bind(s.provider.as_str())
    .bind(&s.host)
    .bind(&s.repo)
    .bind(&s.subject_id)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let source: String = r.try_get("scorer_source")?;
        let detail: String = r.try_get("detail")?;
        out.push(ScoreRecord {
            id: r.try_get("id")?,
            subject: s.clone(),
            scorer_id: r.try_get("scorer_id")?,
            scorer_source: ScorerSource::parse(&source).ok_or_else(|| sqlx::Error::Decode(format!("unknown scorer_source {source}").into()))?,
            rubric_id: r.try_get("rubric_id")?,
            rubric_hash: r.try_get("rubric_hash")?,
            score: r.try_get::<f64, _>("score")? as f32,
            ci_passed: r.try_get("ci_passed")?,
            reverted: r.try_get("reverted")?,
            detail: serde_json::from_str(&detail).unwrap_or(serde_json::Value::Null),
            created_at: r.try_get("created_at")?,
        });
    }
    out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
    Ok(out)
}

/// Median of the scores, rounded to one decimal.
pub fn median(scores: &[f32]) -> Option<f32> {
    if scores.is_empty() {
        return None;
    }
    let mut v: Vec<f32> = scores.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    let m = if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 };
    Some(((m as f64 * 10.0).round() / 10.0) as f32)
}

fn filter_sources(records: Vec<ScoreRecord>, sources: Option<&[ScorerSource]>) -> Vec<ScoreRecord> {
    match sources {
        None => records,
        Some(allowed) => records.into_iter().filter(|r| allowed.contains(&r.scorer_source)).collect(),
    }
}

/// Median score of every submission for the subject; None with fewer than `min_scorers`
/// (and always None with no submissions).
pub async fn aggregate(pool: &SqlitePool, subject: &Subject, min_scorers: usize) -> Result<Option<f32>, sqlx::Error> {
    let recs = list_scores(pool, subject).await?;
    Ok(aggregate_records(&recs, min_scorers))
}

fn aggregate_records(recs: &[ScoreRecord], min_scorers: usize) -> Option<f32> {
    if recs.is_empty() || recs.len() < min_scorers {
        return None;
    }
    median(&recs.iter().map(|r| r.score).collect::<Vec<_>>())
}

/// [`aggregate`] counting only the sources `rules.scorers` allows.
pub async fn aggregate_for_rules(pool: &SqlitePool, subject: &Subject, rules: &MeritRules) -> Result<Option<f32>, sqlx::Error> {
    let recs = filter_sources(list_scores(pool, subject).await?, Some(&rules.scorers));
    Ok(aggregate_records(&recs, rules.min_scorers))
}

/// Once-only key for a subject's score adjustment (also claimed at merge when a
/// score was already used, so the adjustment cannot apply on top of it).
pub fn score_adjust_key(subject: &Subject) -> String {
    provider_key(subject.provider, &subject.host, &format!("score_adjust:{}:{}", subject.repo, subject.subject_id))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScoreAdjustment {
    pub score: f32,
    pub old_points: i32,
    pub new_points: i32,
    pub delta: i32,
}

/// When the first valid aggregate for a merged subject arrives within
/// `score_window_days` after merge, re-price its merit once. Returns None when nothing
/// applies (not merged, not enough scores, outside the window, already applied).
///
/// Does NOT touch the payout ledger: a ledger row keeps the amount computed at merge.
pub async fn apply_score_adjustment(pool: &SqlitePool, subject: &Subject, rules: &MeritRules) -> Result<Option<ScoreAdjustment>, ScoreError> {
    let subject = Subject::new(subject.provider, &subject.host, &subject.repo, &subject.subject_id);
    let Some(row) = load_subject(pool, &subject).await? else { return Ok(None) };
    let merged = crate::models::ContributionStatus::Merged as i64;
    let Some(merged_at) = row.merged_at.filter(|_| row.status == merged) else { return Ok(None) };

    let recs = filter_sources(list_scores(pool, &subject).await?, Some(&rules.scorers));
    let need = rules.min_scorers.max(1);
    if recs.len() < need {
        return Ok(None);
    }
    // The aggregate became valid when the `need`-th submission arrived.
    let valid_at = recs[need - 1].created_at;
    if valid_at > merged_at + Duration::days(rules.score_window_days) {
        return Ok(None);
    }
    let Some(score) = aggregate_records(&recs, need) else { return Ok(None) };

    let mut tx = pool.begin().await?;
    let claimed = sqlx::query("INSERT OR IGNORE INTO processed_webhook_events (event_key) VALUES (?)")
        .bind(score_adjust_key(&subject))
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 1;
    if !claimed {
        tx.rollback().await?;
        return Ok(None);
    }
    let old_points = row.merit_points as i32;
    let new_points = rules.pr_merged.points(Some(score));
    let delta = new_points - old_points;
    sqlx::query("UPDATE contributions SET merit_points = ?, rubric_score = ? WHERE id = ?")
        .bind(new_points)
        .bind(score as f64)
        .bind(row.id)
        .execute(&mut *tx)
        .await?;
    if delta != 0 {
        sqlx::query(
            "INSERT INTO merit_activities (id, contributor_id, activity_type, merit_change, description, metadata,
             created_at, provider, host, repo) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::new_v4())
        .bind(row.contributor_id)
        .bind(ActivityType::PullRequestMerged as i32)
        .bind(delta)
        .bind(format!("Score adjustment for {} {}: {:.1} ({} -> {} points)", subject.repo, subject.subject_id, score, old_points, new_points))
        .bind(Option::<String>::None)
        .bind(Utc::now())
        .bind(subject.provider.as_str())
        .bind(&subject.host)
        .bind(&subject.repo)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE contributors SET total_merit_points = total_merit_points + ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?")
            .bind(delta)
            .bind(row.contributor_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(Some(ScoreAdjustment { score, old_points, new_points, delta }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::gitlab::tests::mr_hook;
    use crate::forge::identity::{find_contributor, link_forge_identity};
    use crate::forge::{gitlab, ForgeProcessor, RepoRegistry};
    use crate::merit::rubric::CRITERIA;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::sync::Arc;

    async fn pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn processor(pool: &SqlitePool, rules: MeritRules) -> ForgeProcessor {
        let reg = RepoRegistry::from_spec("gitlab:gitlab.com:grp/proj").unwrap();
        ForgeProcessor::new(pool.clone()).with_registry(Arc::new(reg)).with_rules(Arc::new(rules))
    }

    async fn gl(p: &ForgeProcessor, action: &str, user: (i64, &str)) {
        let payload: gitlab::GitLabWebhookPayload =
            serde_json::from_value(mr_hook(action, user, 5, "grp/proj", "https://gitlab.com/grp/proj")).unwrap();
        let ev = gitlab::normalize(&payload, None).unwrap().unwrap();
        p.process(&ev).await.unwrap();
    }

    fn subject() -> Subject {
        Subject::new(ForgeProvider::GitLab, "gitlab.com", "Grp/Proj", "555")
    }

    fn agent(id: &str, logins: &[(ForgeProvider, &str)]) -> Scorer {
        Scorer { id: id.into(), source: ScorerSource::Agent, forge_logins: logins.iter().map(|(p, l)| (*p, l.to_string())).collect() }
    }

    fn rubric_sub(level: u8) -> Submission {
        Submission {
            subject: subject(),
            rubric_id: RUBRIC_ID.into(),
            rubric_hash: RUBRIC_SHA256.into(),
            ci_passed: true,
            reverted: false,
            assessment: Assessment::Rubric {
                criteria: CRITERIA
                    .iter()
                    .map(|c| CriterionScore { criterion: c.key.into(), level, evidence: "evidence that is long enough".into() })
                    .collect(),
            },
        }
    }

    async fn total(pool: &SqlitePool, username: &str) -> i64 {
        sqlx::query_scalar("SELECT total_merit_points FROM contributors WHERE github_username = ?")
            .bind(username)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn self_score_refused_directly_and_through_linked_identity() {
        let pool = pool().await;
        let p = processor(&pool, MeritRules::default());
        gl(&p, "open", (5, "carol")).await;

        // Same forge login as the author (case-insensitive)
        let e = submit_score(&pool, &rubric_sub(3), &agent("agent-carol", &[(ForgeProvider::GitLab, "Carol")])).await;
        assert!(matches!(e, Err(ScoreError::SelfScore)), "{e:?}");

        // A different login linked to the same contributor
        let carol = find_contributor(&pool, ForgeProvider::GitLab, "gitlab.com", "carol").await.unwrap().unwrap();
        link_forge_identity(&pool, carol.id, ForgeProvider::GitHub, "github.com", "carol-gh").await.unwrap();
        let e = submit_score(&pool, &rubric_sub(3), &agent("agent-x", &[(ForgeProvider::GitHub, "carol-gh")])).await;
        assert!(matches!(e, Err(ScoreError::SelfScore)), "{e:?}");

        // Control: an unrelated scorer is accepted
        let ok = submit_score(&pool, &rubric_sub(3), &agent("agent-dave", &[(ForgeProvider::GitLab, "dave")])).await.unwrap();
        assert_eq!(ok.score, 75.0);
        assert_eq!(list_scores(&pool, &subject()).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn duplicate_scorer_refused_and_unknown_subject_refused() {
        let pool = pool().await;
        let p = processor(&pool, MeritRules::default());
        gl(&p, "open", (5, "carol")).await;
        let s = agent("agent-1", &[]);
        submit_score(&pool, &rubric_sub(2), &s).await.unwrap();
        let e = submit_score(&pool, &rubric_sub(4), &s).await;
        assert!(matches!(e, Err(ScoreError::DuplicateScorer)), "{e:?}");
        assert_eq!(list_scores(&pool, &subject()).await.unwrap().len(), 1);

        let mut other = rubric_sub(2);
        other.subject = Subject::new(ForgeProvider::GitLab, "gitlab.com", "grp/proj", "999");
        assert!(matches!(submit_score(&pool, &other, &s).await, Err(ScoreError::UnknownSubject)));
    }

    #[tokio::test]
    async fn submission_shape_and_header_enforced() {
        let pool = pool().await;
        let p = processor(&pool, MeritRules::default());
        gl(&p, "open", (5, "carol")).await;
        let mut bad = rubric_sub(2);
        bad.rubric_hash = "0".repeat(64);
        assert!(matches!(submit_score(&pool, &bad, &agent("a", &[])).await, Err(ScoreError::Rubric(RubricError::HashMismatch))));
        let total = crate::services::coderabbit::coderabbit_submission(subject(), 88.0, "CodeRabbit review summary text".into(), true, false);
        assert!(matches!(submit_score(&pool, &total, &agent("a", &[])).await, Err(ScoreError::WrongShape(_))));
        let rec = submit_score(&pool, &total, &crate::services::coderabbit::coderabbit_scorer()).await.unwrap();
        assert_eq!(rec.score, 88.0);
        assert_eq!(rec.scorer_source, ScorerSource::CodeRabbit);
    }

    #[test]
    fn median_values() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[70.0]), Some(70.0));
        assert_eq!(median(&[90.0, 10.0, 50.0]), Some(50.0));
        assert_eq!(median(&[10.0, 20.0, 30.0, 100.0]), Some(25.0));
    }

    #[tokio::test]
    async fn aggregate_is_median_and_respects_min_scorers() {
        let pool = pool().await;
        let p = processor(&pool, MeritRules::default());
        gl(&p, "open", (5, "carol")).await;
        assert_eq!(aggregate(&pool, &subject(), 1).await.unwrap(), None);
        submit_score(&pool, &rubric_sub(4), &agent("a1", &[])).await.unwrap(); // 100
        assert_eq!(aggregate(&pool, &subject(), 1).await.unwrap(), Some(100.0));
        assert_eq!(aggregate(&pool, &subject(), 2).await.unwrap(), None);
        submit_score(&pool, &rubric_sub(0), &agent("a2", &[])).await.unwrap(); // 0
        assert_eq!(aggregate(&pool, &subject(), 2).await.unwrap(), Some(50.0));
        submit_score(&pool, &rubric_sub(1), &agent("a3", &[])).await.unwrap(); // 25
        assert_eq!(aggregate(&pool, &subject(), 3).await.unwrap(), Some(25.0));
        assert_eq!(aggregate(&pool, &subject(), 4).await.unwrap(), None);
    }

    #[tokio::test]
    async fn score_adjustment_applies_once_after_merge() {
        let pool = pool().await;
        let rules = MeritRules::default();
        let p = processor(&pool, rules.clone());
        gl(&p, "open", (5, "carol")).await;
        gl(&p, "merge", (5, "carol")).await; // no score at merge: base 10
        assert_eq!(total(&pool, "gitlab:carol").await, 5 + 10);
        assert_eq!(apply_score_adjustment(&pool, &subject(), &rules).await.unwrap(), None, "no score yet");

        submit_score(&pool, &rubric_sub(4), &agent("a1", &[])).await.unwrap();
        let adj = apply_score_adjustment(&pool, &subject(), &rules).await.unwrap().unwrap();
        assert_eq!(adj, ScoreAdjustment { score: 100.0, old_points: 10, new_points: 60, delta: 50 });
        assert_eq!(total(&pool, "gitlab:carol").await, 5 + 60);

        // Idempotent: a second call (or a later score) does nothing more
        assert_eq!(apply_score_adjustment(&pool, &subject(), &rules).await.unwrap(), None);
        submit_score(&pool, &rubric_sub(0), &agent("a2", &[])).await.unwrap();
        assert_eq!(apply_score_adjustment(&pool, &subject(), &rules).await.unwrap(), None);
        assert_eq!(total(&pool, "gitlab:carol").await, 5 + 60);
        let (pts, score): (i64, f64) = sqlx::query_as("SELECT merit_points, rubric_score FROM contributions").fetch_one(&pool).await.unwrap();
        assert_eq!((pts, score), (60, 100.0));
        let repo: String = sqlx::query_scalar("SELECT repo FROM merit_activities WHERE description LIKE 'Score adjustment%'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(repo, "grp/proj");
    }

    #[tokio::test]
    async fn score_outside_window_or_before_merge_does_not_adjust() {
        let pool = pool().await;
        let rules = MeritRules::default();
        let p = processor(&pool, rules.clone());
        gl(&p, "open", (5, "carol")).await;
        assert_eq!(apply_score_adjustment(&pool, &subject(), &rules).await.unwrap(), None, "not merged");
        gl(&p, "merge", (5, "carol")).await;
        sqlx::query("UPDATE contributions SET merged_at = ?").bind(Utc::now() - Duration::days(8)).execute(&pool).await.unwrap();
        submit_score(&pool, &rubric_sub(4), &agent("a1", &[])).await.unwrap();
        assert_eq!(apply_score_adjustment(&pool, &subject(), &rules).await.unwrap(), None, "outside 7-day window");
        assert_eq!(total(&pool, "gitlab:carol").await, 15);
    }

    #[tokio::test]
    async fn score_present_at_merge_is_used_and_not_applied_twice() {
        let pool = pool().await;
        let rules = MeritRules::default();
        let p = processor(&pool, rules.clone());
        gl(&p, "open", (5, "carol")).await;
        submit_score(&pool, &rubric_sub(2), &agent("a1", &[])).await.unwrap(); // 50 -> 35 points
        gl(&p, "merge", (5, "carol")).await;
        assert_eq!(total(&pool, "gitlab:carol").await, 5 + 35);
        assert_eq!(apply_score_adjustment(&pool, &subject(), &rules).await.unwrap(), None);
        assert_eq!(total(&pool, "gitlab:carol").await, 5 + 35);
    }

    #[tokio::test]
    async fn scores_from_sources_not_in_rules_do_not_count() {
        let pool = pool().await;
        let rules = MeritRules::default(); // scorers = [agent]
        let p = processor(&pool, rules.clone());
        gl(&p, "open", (5, "carol")).await;
        let cr = crate::services::coderabbit::coderabbit_submission(subject(), 100.0, "CodeRabbit review summary text".into(), true, false);
        submit_score(&pool, &cr, &crate::services::coderabbit::coderabbit_scorer()).await.unwrap();
        gl(&p, "merge", (5, "carol")).await;
        assert_eq!(total(&pool, "gitlab:carol").await, 5 + 10, "coderabbit not enabled: base points");
    }
}
