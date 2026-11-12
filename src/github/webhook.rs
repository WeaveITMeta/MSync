use crate::forge::{self, ForgeProcessor, ProcessOutcome, RepoRegistry};
use crate::merit::scoring::MeritRules;
use crate::payments::PaymentConfig;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::sync::Arc;
use tracing::info;

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

/// GitHub webhook entry: normalizes the payload to a `ForgeEvent` and runs the shared
/// merit and payout path (`forge::ForgeProcessor`).
pub struct GitHubWebhookHandler {
    processor: ForgeProcessor,
}

impl GitHubWebhookHandler {
    /// Legacy registry (every GitHub repository earns), default merit rules, no payments.
    pub fn new(db_pool: SqlitePool) -> Self {
        Self { processor: ForgeProcessor::new(db_pool) }
    }

    pub fn with_payments(mut self, cfg: Arc<PaymentConfig>) -> Self {
        self.processor = self.processor.with_payments(cfg);
        self
    }

    pub fn with_registry(mut self, registry: Arc<RepoRegistry>) -> Self {
        self.processor = self.processor.with_registry(registry);
        self
    }

    pub fn with_rules(mut self, rules: Arc<MeritRules>) -> Self {
        self.processor = self.processor.with_rules(rules);
        self
    }

    pub async fn handle_webhook(&self, event_type: &str, payload: GitHubWebhookPayload) -> Result<()> {
        self.handle_delivery(event_type, payload, None).await.map(|_| ())
    }

    pub async fn handle_delivery(&self, event_type: &str, payload: GitHubWebhookPayload, delivery: Option<&str>) -> Result<Option<ProcessOutcome>> {
        match forge::github::normalize(event_type, &payload, delivery) {
            Some(ev) => Ok(Some(self.processor.process(&ev).await?)),
            None => {
                info!("Unhandled webhook event type/action: {} {:?}", event_type, payload.action);
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod dedupe_tests {
    use super::*;
    use crate::github::dedupe;
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
        let h = GitHubWebhookHandler::new(pool.clone()).with_payments(cfg);

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
        let h = GitHubWebhookHandler::new(pool.clone());
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

    async fn state_counts(pool: &SqlitePool) -> (i64, i64, i64, i64) {
        let c = |q: &'static str| async move { sqlx::query_scalar::<_, i64>(q).fetch_one(pool).await.unwrap() };
        (
            c("SELECT COALESCE(SUM(total_merit_points), 0) FROM contributors").await,
            c("SELECT COUNT(*) FROM merit_activities").await,
            c("SELECT COUNT(*) FROM contributions").await,
            c("SELECT COUNT(*) FROM payout_ledger").await,
        )
    }

    async fn run_full_flow(h: &GitHubWebhookHandler) {
        h.handle_webhook("pull_request", pr_payload("opened", false)).await.unwrap();
        h.handle_webhook("pull_request", pr_payload("closed", true)).await.unwrap();
        let mut review = pr_payload("submitted", false);
        review.review = Some(ReviewPayload { state: "approved".into(), body: None, user: GitHubUser { login: "bob".into() } });
        h.handle_webhook("pull_request_review", review).await.unwrap();
    }

    #[tokio::test]
    async fn unlisted_and_mirror_github_repos_earn_nothing() {
        let cfg = Arc::new(PaymentConfig::from_lookup(|_| None));
        for spec in ["github:github.com:org/other", "github:github.com:org/eustress:mirror"] {
            let pool = pool().await;
            let reg = Arc::new(RepoRegistry::from_spec(spec).unwrap());
            let h = GitHubWebhookHandler::new(pool.clone()).with_payments(cfg.clone()).with_registry(reg);
            run_full_flow(&h).await;
            assert_eq!(state_counts(&pool).await, (0, 0, 0, 0), "{spec}: repo must earn nothing and create no ledger row");
        }
        // Control: the same flow on a listed canonical repo earns and creates one ledger row.
        let pool = pool().await;
        let reg = Arc::new(RepoRegistry::from_spec("github:github.com:org/eustress").unwrap());
        let h = GitHubWebhookHandler::new(pool.clone()).with_payments(cfg).with_registry(reg);
        run_full_flow(&h).await;
        let (pts, acts, contribs, ledger) = state_counts(&pool).await;
        assert_eq!((pts, acts, contribs, ledger), (5 + 10 + 10, 3, 1, 1));
    }

    #[tokio::test]
    async fn merge_without_score_earns_base_points_and_same_pr_number_in_two_repos_does_not_collide() {
        let pool = pool().await;
        let h = GitHubWebhookHandler::new(pool.clone());
        let mut other = pr_payload("opened", false);
        other.repository = Some(RepositoryPayload { full_name: "org/other".into() });
        other.pull_request.as_mut().unwrap().id = 5555; // same number 7, different repo
        h.handle_webhook("pull_request", other).await.unwrap();
        h.handle_webhook("pull_request", pr_payload("opened", false)).await.unwrap();
        h.handle_webhook("pull_request", pr_payload("closed", true)).await.unwrap();
        let rows: Vec<(String, i64, i64)> = sqlx::query_as("SELECT repo, status, merit_points FROM contributions ORDER BY repo")
            .fetch_all(&pool).await.unwrap();
        assert_eq!(rows, vec![("org/eustress".into(), 3, 10), ("org/other".into(), 0, 0)]);
    }
}
