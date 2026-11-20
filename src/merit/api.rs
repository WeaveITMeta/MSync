//! Merit operations shared by REST (`/api/v1/merit/*`, `/api/v1/leaderboard`,
//! `/api/v1/members/{id}/merit`) and the MCP server (`/mcp`). Contract sections 5.1, 6 and 7.
//!
//! The plain async functions here are the only implementation. The REST handlers at the bottom
//! and the MCP tools in `crate::mcp::tools` are thin adapters over them; neither reimplements the
//! other. The scorer is always built from the authenticated token, never from a request body.

use super::leaderboard::{self, LeaderboardQuery, LeaderboardRow, MemberMerit};
use super::rubric::{self, CriterionScore, CRITERIA, RUBRIC_ID, RUBRIC_SHA256};
use super::scoring::{self, Assessment, MeritRules, ScoreAdjustment, ScoreError, ScoreRecord, Scorer, ScorerSource, Submission};
use crate::auth::{authenticate, member_payee_keys, scopes, AuthError, Principal};
use crate::foundry::{FoundryProvider, RepoRef, RepoRegistry, Subject};
use crate::payments::api::auth_error;
use actix_web::{get, post, web, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::sync::Arc;
use tracing::{error, warn};
use uuid::Uuid;

pub const DEFAULT_LEADERBOARD_LIMIT: usize = 25;
pub const MAX_LEADERBOARD_LIMIT: usize = 500;

// ------------------------------------------------------------------ context

/// Where the repository registry and the merit rules come from. Read per call so a later round
/// (the CRA) can change them at runtime through the seams in `crate::routes`.
pub trait MeritSeams: Send + Sync {
    fn registry(&self) -> Arc<RepoRegistry>;
    fn rules(&self) -> Arc<MeritRules>;
}

/// Fixed registry and rules (tests, tools).
pub struct FixedSeams {
    pub registry: Arc<RepoRegistry>,
    pub rules: Arc<MeritRules>,
}

impl MeritSeams for FixedSeams {
    fn registry(&self) -> Arc<RepoRegistry> {
        self.registry.clone()
    }
    fn rules(&self) -> Arc<MeritRules> {
        self.rules.clone()
    }
}

/// The production seams: `routes::foundry_registry` and `routes::merit_rules` over the app state.
pub struct AppStateSeams(pub web::Data<crate::AppState>);

impl MeritSeams for AppStateSeams {
    fn registry(&self) -> Arc<RepoRegistry> {
        crate::routes::foundry_registry(&self.0)
    }
    fn rules(&self) -> Arc<MeritRules> {
        crate::routes::merit_rules(&self.0)
    }
}

#[derive(Clone)]
pub struct MeritContext {
    pub pool: SqlitePool,
    pub seams: Arc<dyn MeritSeams>,
}

// ------------------------------------------------------------------ errors

/// Every message is safe to show a caller: never SQL text, a path, a token or a stack trace.
/// Internal detail is logged and replaced by "internal error".
#[derive(Debug)]
pub enum MeritApiError {
    Auth(AuthError),
    Invalid(String),
    NotFound(String),
    Forbidden(String),
    Conflict(String),
    Internal,
}

impl MeritApiError {
    pub fn message(&self) -> String {
        match self {
            MeritApiError::Auth(AuthError::MissingScope(s)) => format!("missing scope {s}"),
            MeritApiError::Auth(_) => "unauthorized".into(),
            MeritApiError::Invalid(m) | MeritApiError::NotFound(m) | MeritApiError::Forbidden(m) | MeritApiError::Conflict(m) => m.clone(),
            MeritApiError::Internal => "internal error".into(),
        }
    }
}

fn internal(context: &str, e: impl std::fmt::Display) -> MeritApiError {
    error!("{context}: {e}");
    MeritApiError::Internal
}

impl From<AuthError> for MeritApiError {
    fn from(e: AuthError) -> Self {
        MeritApiError::Auth(e)
    }
}

impl From<ScoreError> for MeritApiError {
    fn from(e: ScoreError) -> Self {
        let msg = e.to_string();
        match e {
            ScoreError::Rubric(r) => MeritApiError::Invalid(r.to_string()),
            ScoreError::WrongShape(m) => MeritApiError::Invalid(m.to_string()),
            ScoreError::EmptyScorerId => MeritApiError::Invalid("scorer id is empty".into()),
            ScoreError::UnknownSubject => MeritApiError::NotFound(msg),
            ScoreError::SelfScore => MeritApiError::Forbidden(msg),
            ScoreError::DuplicateScorer => MeritApiError::Conflict(msg),
            ScoreError::Db(d) => internal("merit score db error", d),
        }
    }
}

// ------------------------------------------------------------------ inputs

/// A subject id given as a string ("555", "n12") or a number (555).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SubjectIdInput {
    Text(String),
    Number(i64),
}

impl SubjectIdInput {
    fn into_string(self) -> String {
        match self {
            SubjectIdInput::Text(s) => s,
            SubjectIdInput::Number(n) => n.to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubjectInput {
    pub provider: String,
    /// Defaults to the provider's public host (github.com, gitlab.com).
    #[serde(default)]
    pub host: Option<String>,
    pub repo: String,
    pub subject_id: SubjectIdInput,
}

fn parse_provider(p: &str) -> Result<FoundryProvider, MeritApiError> {
    FoundryProvider::parse(p).ok_or_else(|| MeritApiError::Invalid(format!("unknown provider '{}' (github or gitlab)", p.trim())))
}

fn host_or_default(provider: FoundryProvider, host: Option<&str>) -> String {
    match host.map(str::trim).filter(|h| !h.is_empty()) {
        Some(h) => h.to_string(),
        None => provider.default_host().to_string(),
    }
}

impl SubjectInput {
    pub fn to_subject(&self) -> Result<Subject, MeritApiError> {
        let provider = parse_provider(&self.provider)?;
        let host = host_or_default(provider, self.host.as_deref());
        let sid = self.subject_id.clone().into_string();
        if self.repo.trim().trim_matches('/').is_empty() {
            return Err(MeritApiError::Invalid("repo is required".into()));
        }
        if sid.trim().is_empty() {
            return Err(MeritApiError::Invalid("subject_id is required".into()));
        }
        Ok(Subject::new(provider, &host, &self.repo, &sid))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LevelInput {
    pub level: u8,
    pub evidence: String,
}

/// All six criteria of `msync-rubric-v1`, each exactly once.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CriteriaInput {
    pub impact: LevelInput,
    pub correctness: LevelInput,
    pub difficulty: LevelInput,
    pub quality: LevelInput,
    pub risk: LevelInput,
    pub communication: LevelInput,
}

impl CriteriaInput {
    fn into_scores(self) -> Vec<CriterionScore> {
        [
            ("impact", self.impact),
            ("correctness", self.correctness),
            ("difficulty", self.difficulty),
            ("quality", self.quality),
            ("risk", self.risk),
            ("communication", self.communication),
        ]
        .into_iter()
        .map(|(k, l)| CriterionScore { criterion: k.to_string(), level: l.level, evidence: l.evidence })
        .collect()
    }
}

/// A rubric score submission. There is no scorer, actor or author field: the scorer is the
/// token's member, and unknown fields are rejected.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScoreInput {
    pub subject: SubjectInput,
    pub rubric_id: String,
    pub rubric_hash: String,
    pub ci_passed: bool,
    pub reverted: bool,
    pub criteria: CriteriaInput,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaderboardParams {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

// ------------------------------------------------------------------ scorer

/// GitHub and GitLab logins derived from payee keys: a key starting `gitlab:` is a GitLab login, anything
/// else a GitHub login. Host-qualified keys (`gitlab:<host>:<login>`, `github:<host>:<login>`,
/// the `legacy_username` forms) yield the login after the last ':'.
pub fn foundry_logins_from_payee_keys(keys: &[String]) -> Vec<(FoundryProvider, String)> {
    let mut out: Vec<(FoundryProvider, String)> = Vec::new();
    for raw in keys {
        let k = raw.trim().to_ascii_lowercase();
        let (provider, rest) = if let Some(rest) = k.strip_prefix("gitlab:") {
            (FoundryProvider::GitLab, rest)
        } else if let Some(rest) = k.strip_prefix("github:") {
            (FoundryProvider::GitHub, rest)
        } else {
            (FoundryProvider::GitHub, k.as_str())
        };
        let login = rest.rsplit(':').next().unwrap_or("").trim().to_string();
        if login.is_empty() {
            continue;
        }
        if !out.iter().any(|(p, l)| *p == provider && *l == login) {
            out.push((provider, login));
        }
    }
    out
}

/// The scorer for a principal: id = member id, source = Agent, GitHub and GitLab logins from the payee keys
/// of this token and of every other token the member holds.
///
/// Known limit (round 2): a member whose tokens declare no payee keys has no GitHub and GitLab logins and is
/// not recognized as the author of their own work. The CRA supplies identities in a later round.
pub async fn scorer_for(pool: &SqlitePool, principal: &Principal) -> Result<Scorer, MeritApiError> {
    let mut keys = principal.payee_keys.clone();
    for k in member_payee_keys(pool, &principal.member_id).await.map_err(|e| internal("payee keys lookup", e))? {
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    Ok(Scorer { id: principal.member_id.clone(), source: ScorerSource::Agent, foundry_logins: foundry_logins_from_payee_keys(&keys) })
}

// ------------------------------------------------------------------ operations

/// The rubric, its pinned hash, the gates, the points formula and the event rules in force.
pub fn rubric_view(rules: &MeritRules) -> Value {
    let criteria: Vec<Value> = CRITERIA
        .iter()
        .map(|c| {
            json!({
                "key": c.key,
                "weight": c.weight,
                "levels": c.levels.iter().enumerate().map(|(i, t)| json!({"level": i, "meaning": t})).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "id": RUBRIC_ID,
        "hash": RUBRIC_SHA256,
        "hash_algorithm": "sha256 of the canonical rubric JSON",
        "criteria": criteria,
        "levels": {"min": rubric::MIN_LEVEL, "max": rubric::MAX_LEVEL},
        "min_evidence_chars": rubric::MIN_EVIDENCE_CHARS,
        "score_formula": "score = sum(weight * level / 4), 0..100, rounded to one decimal",
        "gates": {
            "ci_passed_false_caps_score_at": rubric::CI_FAILED_CAP,
            "reverted_true_sets_score_to": 0,
        },
        "points_formula": "points = base_points + round((max_points - base_points) * score / 100)",
        "aggregation": "median of the counted scorers' scores; fewer than min_scorers means base points",
        "rules": {
            "rubric": rules.rubric,
            "scorers": rules.scorers,
            "min_scorers": rules.min_scorers,
            "score_window_days": rules.score_window_days,
            "events": {
                "pr_opened": rules.pr_opened,
                "pr_merged": rules.pr_merged,
                "review_approved": rules.review_approved,
            },
        },
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct AggregateView {
    /// Median of the counted scores, or None with fewer than `min_scorers`.
    pub score: Option<f32>,
    /// Submissions from sources the merit rules count.
    pub counted_scorers: usize,
    pub min_scorers: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScoreSubmitResult {
    pub score: ScoreRecord,
    pub aggregate: AggregateView,
    /// The once-only merit re-pricing this submission caused, if any.
    pub merit_adjustment: Option<ScoreAdjustment>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScoresView {
    pub subject: Subject,
    pub scores: Vec<ScoreRecord>,
    pub aggregate: AggregateView,
}

async fn aggregate_view(pool: &SqlitePool, subject: &Subject, rules: &MeritRules) -> Result<AggregateView, MeritApiError> {
    let recs = scoring::list_scores(pool, subject).await.map_err(|e| internal("list scores", e))?;
    let counted = recs.iter().filter(|r| rules.scorers.contains(&r.scorer_source)).count();
    let score = scoring::aggregate_for_rules(pool, subject, rules).await.map_err(|e| internal("aggregate scores", e))?;
    Ok(AggregateView { score, counted_scorers: counted, min_scorers: rules.min_scorers })
}

/// Submit a rubric score as the token's member (`merit.score`), then apply the once-only merit
/// adjustment the score may trigger.
pub async fn submit_score(ctx: &MeritContext, principal: &Principal, input: ScoreInput) -> Result<ScoreSubmitResult, MeritApiError> {
    principal.require(scopes::MERIT_SCORE)?;
    let rules = ctx.seams.rules();
    let subject = input.subject.to_subject()?;
    let scorer = scorer_for(&ctx.pool, principal).await?;
    let sub = Submission {
        subject: subject.clone(),
        rubric_id: input.rubric_id,
        rubric_hash: input.rubric_hash,
        ci_passed: input.ci_passed,
        reverted: input.reverted,
        assessment: Assessment::Rubric { criteria: input.criteria.into_scores() },
    };
    let record = scoring::submit_score(&ctx.pool, &sub, &scorer).await?;
    let adjustment = scoring::apply_score_adjustment(&ctx.pool, &subject, &rules).await?;
    let aggregate = aggregate_view(&ctx.pool, &subject, &rules).await?;
    Ok(ScoreSubmitResult { score: record, aggregate, merit_adjustment: adjustment })
}

/// Every score submitted for a subject, and the aggregate the merit rules use (`merit.read`).
pub async fn list_scores(ctx: &MeritContext, principal: &Principal, subject: &SubjectInput) -> Result<ScoresView, MeritApiError> {
    principal.require(scopes::MERIT_READ)?;
    let subject = subject.to_subject()?;
    let rules = ctx.seams.rules();
    let scores = scoring::list_scores(&ctx.pool, &subject).await.map_err(|e| internal("list scores", e))?;
    let aggregate = aggregate_view(&ctx.pool, &subject, &rules).await?;
    Ok(ScoresView { subject, scores, aggregate })
}

#[derive(Debug, Clone, Serialize)]
pub struct LeaderboardView {
    /// The repository ranked alone, or None for totals across every repository.
    pub repo: Option<RepoRef>,
    pub limit: usize,
    pub include_private_names: bool,
    pub leaderboard: Vec<LeaderboardRow>,
}

/// Leaderboard with private repository names (`merit.read`): totals, or one repository's XP.
pub async fn leaderboard_view(ctx: &MeritContext, principal: &Principal, params: &LeaderboardParams) -> Result<LeaderboardView, MeritApiError> {
    principal.require(scopes::MERIT_READ)?;
    let limit = params.limit.unwrap_or(DEFAULT_LEADERBOARD_LIMIT);
    if limit == 0 || limit > MAX_LEADERBOARD_LIMIT {
        return Err(MeritApiError::Invalid(format!("limit must be 1..={MAX_LEADERBOARD_LIMIT}")));
    }
    let nonempty = |o: &Option<String>| o.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let repo = match (nonempty(&params.provider), nonempty(&params.host), nonempty(&params.repo)) {
        (None, None, None) => None,
        (Some(p), h, Some(r)) => {
            let provider = parse_provider(&p)?;
            Some(RepoRef::new(provider, &host_or_default(provider, h.as_deref()), &r))
        }
        (None, _, Some(_)) => return Err(MeritApiError::Invalid("provider is required with repo".into())),
        (_, _, None) => return Err(MeritApiError::Invalid("repo is required with provider or host".into())),
    };
    let registry = ctx.seams.registry();
    let q = LeaderboardQuery { repo: repo.clone(), limit, include_private_names: true };
    let rows = leaderboard::leaderboard(&ctx.pool, &registry, q).await.map_err(|e| internal("leaderboard", e))?;
    Ok(LeaderboardView { repo, limit, include_private_names: true, leaderboard: rows })
}

/// One person's XP by repository and by event kind, private names included (`merit.read`).
pub async fn member_merit(ctx: &MeritContext, principal: &Principal, contributor_id: &str) -> Result<MemberMerit, MeritApiError> {
    principal.require(scopes::MERIT_READ)?;
    let id = Uuid::parse_str(contributor_id.trim())
        .map_err(|_| MeritApiError::Invalid("contributor_id must be a UUID (the contributor_id field of a leaderboard row)".into()))?;
    let registry = ctx.seams.registry();
    leaderboard::member_merit(&ctx.pool, &registry, id, true)
        .await
        .map_err(|e| internal("member merit", e))?
        .ok_or_else(|| MeritApiError::NotFound("member not found".into()))
}

/// JSON for a result, through text so `f32` scores keep their short form (`31.3`, not
/// `31.299999237060547`, which `serde_json::to_value` produces). REST and MCP both use it.
pub fn to_json<T: Serialize>(v: &T) -> Result<Value, MeritApiError> {
    let s = serde_json::to_string(v).map_err(|e| internal("result serialization", e))?;
    serde_json::from_str(&s).map_err(|e| internal("result serialization", e))
}

// ------------------------------------------------------------------ REST

fn rest_ok<T: Serialize>(status: actix_web::http::StatusCode, v: &T) -> HttpResponse {
    match to_json(v) {
        Ok(result) => HttpResponse::build(status).json(json!({"status": "success", "result": result})),
        Err(e) => merit_error(e),
    }
}

pub fn merit_error(e: MeritApiError) -> HttpResponse {
    use actix_web::http::StatusCode as S;
    let status = match &e {
        MeritApiError::Auth(a) => return auth_error(a),
        MeritApiError::Invalid(_) => S::BAD_REQUEST,
        MeritApiError::NotFound(_) => S::NOT_FOUND,
        MeritApiError::Forbidden(_) => S::FORBIDDEN,
        MeritApiError::Conflict(_) => S::CONFLICT,
        MeritApiError::Internal => S::INTERNAL_SERVER_ERROR,
    };
    HttpResponse::build(status).json(json!({"status": "error", "message": e.message()}))
}

async fn rest_principal(req: &HttpRequest, pool: &SqlitePool) -> Result<Principal, HttpResponse> {
    authenticate(req, pool).await.map_err(|e| {
        warn!("v1 merit auth refused: {}", e);
        auth_error(&e)
    })
}

fn bad_request(msg: String) -> HttpResponse {
    HttpResponse::BadRequest().json(json!({"status": "error", "message": msg}))
}

#[get("/api/v1/merit/rubric")]
pub async fn get_rubric(ctx: web::Data<MeritContext>) -> HttpResponse {
    HttpResponse::Ok().json(json!({"status": "success", "rubric": rubric_view(&ctx.seams.rules())}))
}

#[post("/api/v1/merit/scores")]
pub async fn post_score(req: HttpRequest, body: web::Bytes, ctx: web::Data<MeritContext>) -> HttpResponse {
    let p = match rest_principal(&req, &ctx.pool).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let input: ScoreInput = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return bad_request(format!("invalid score submission: {e}")),
    };
    match submit_score(&ctx, &p, input).await {
        Ok(r) => rest_ok(actix_web::http::StatusCode::CREATED, &r),
        Err(e) => merit_error(e),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScoresQuery {
    provider: String,
    #[serde(default)]
    host: Option<String>,
    repo: String,
    subject_id: String,
}

#[get("/api/v1/merit/scores")]
pub async fn get_scores(req: HttpRequest, ctx: web::Data<MeritContext>) -> HttpResponse {
    let p = match rest_principal(&req, &ctx.pool).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let q = match web::Query::<ScoresQuery>::from_query(req.query_string()) {
        Ok(q) => q.into_inner(),
        Err(e) => return bad_request(format!("invalid query: {e}")),
    };
    let subject = SubjectInput { provider: q.provider, host: q.host, repo: q.repo, subject_id: SubjectIdInput::Text(q.subject_id) };
    match list_scores(&ctx, &p, &subject).await {
        Ok(v) => rest_ok(actix_web::http::StatusCode::OK, &v),
        Err(e) => merit_error(e),
    }
}

#[get("/api/v1/leaderboard")]
pub async fn get_leaderboard(req: HttpRequest, ctx: web::Data<MeritContext>) -> HttpResponse {
    let p = match rest_principal(&req, &ctx.pool).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let q = match web::Query::<LeaderboardParams>::from_query(req.query_string()) {
        Ok(q) => q.into_inner(),
        Err(e) => return bad_request(format!("invalid query: {e}")),
    };
    match leaderboard_view(&ctx, &p, &q).await {
        Ok(v) => rest_ok(actix_web::http::StatusCode::OK, &v),
        Err(e) => merit_error(e),
    }
}

#[get("/api/v1/members/{contributor_id}/merit")]
pub async fn get_member_merit(req: HttpRequest, path: web::Path<String>, ctx: web::Data<MeritContext>) -> HttpResponse {
    let p = match rest_principal(&req, &ctx.pool).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    match member_merit(&ctx, &p, &path).await {
        Ok(v) => rest_ok(actix_web::http::StatusCode::OK, &v),
        Err(e) => merit_error(e),
    }
}

/// Register the v1 merit and leaderboard routes. Needs `web::Data<MeritContext>`.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(get_rubric).service(post_score).service(get_scores).service(get_leaderboard).service(get_member_merit);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payee_keys_map_to_foundry_logins() {
        let keys: Vec<String> = ["Alice", "gitlab:Carol", "gitlab:git.example.org:dave", "github:ghe.example.org:erin", "  ", "gitlab:", "alice"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            foundry_logins_from_payee_keys(&keys),
            vec![
                (FoundryProvider::GitHub, "alice".to_string()),
                (FoundryProvider::GitLab, "carol".to_string()),
                (FoundryProvider::GitLab, "dave".to_string()),
                (FoundryProvider::GitHub, "erin".to_string()),
            ]
        );
        assert!(foundry_logins_from_payee_keys(&[]).is_empty());
    }
}
