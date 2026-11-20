//! CRA loading: the loader against a fake source, the GitHub and GitLab sources against
//! `mockito`, the push trigger, the seams, and the Eustress Engine story.

use super::*;
use crate::auth::{scopes, CraDirectory, Directory, EnvDirectory, Principal, DEFAULT_ORG};
use crate::cra::tests::{add_grant, base, edit, today, F};
use crate::cra::{CraFiles, GitHubSource, GitLabSource};
use crate::foundry::{self as foundry_mod};
use crate::payments::{approvals, ledger, policy as pay_policy, PaymentConfig};
use chrono::Utc;
use mockito::Matcher;
use serde_json::json;
use sqlx::sqlite::SqlitePoolOptions;
use std::sync::Mutex;

// ---------------------------------------------------------------- fixtures

async fn pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    pool
}

fn sha(n: u64) -> String {
    format!("{:040x}", n)
}

fn repo() -> CraRepo {
    CraRepo::new(FoundryProvider::GitHub, "github.com", "example/cra")
}

fn gh(login: &str) -> FoundryIdentity {
    FoundryIdentity::github(login)
}

fn owners() -> Vec<String> {
    vec!["github:test-owner".to_string()]
}

fn direct(actor: &str) -> ChangeContext {
    ChangeContext {
        actor: gh(actor),
        via_pull_request: false,
        author: None,
        approvers: vec![],
        today: today(),
    }
}

fn via_pr(actor: &str, author: &str, approvers: &[&str]) -> ChangeContext {
    ChangeContext {
        actor: gh(actor),
        via_pull_request: true,
        author: Some(gh(author)),
        approvers: approvers.iter().map(|a| gh(a)).collect(),
        today: today(),
    }
}

type Commit = (
    Result<CraFiles, SourceError>,
    Result<ChangeContext, SourceError>,
);

/// A source whose answers are set per commit. Records every SHA it was asked for.
struct FakeSource {
    repo: CraRepo,
    commits: Mutex<HashMap<String, Commit>>,
    fetched: Mutex<Vec<String>>,
    head: Mutex<Option<String>>,
}

impl FakeSource {
    fn new() -> Self {
        FakeSource {
            repo: repo(),
            commits: Mutex::new(HashMap::new()),
            fetched: Mutex::new(Vec::new()),
            head: Mutex::new(None),
        }
    }
    fn commit(
        &self,
        sha: &str,
        files: Result<CraFiles, SourceError>,
        ctx: Result<ChangeContext, SourceError>,
    ) {
        self.commits
            .lock()
            .unwrap()
            .insert(sha.to_string(), (files, ctx));
    }
    fn fetched(&self) -> Vec<String> {
        self.fetched.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl CraSource for FakeSource {
    fn repo(&self) -> &CraRepo {
        &self.repo
    }
    async fn fetch_files(&self, sha: &str) -> Result<CraFiles, SourceError> {
        self.fetched.lock().unwrap().push(sha.to_string());
        match self.commits.lock().unwrap().get(sha) {
            Some((f, _)) => f.clone(),
            None => Err(SourceError::Api(format!("no commit {sha}"))),
        }
    }
    async fn change_context(
        &self,
        sha: &str,
        _pusher: Option<&FoundryIdentity>,
        _today: NaiveDate,
    ) -> Result<ChangeContext, SourceError> {
        match self.commits.lock().unwrap().get(sha) {
            Some((_, c)) => c.clone(),
            None => Err(SourceError::Api(format!("no commit {sha}"))),
        }
    }
    async fn branch_head(&self, _branch: &str) -> Result<String, SourceError> {
        self.head
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| SourceError::Api("no head".into()))
    }
}

async fn load(pool: &SqlitePool, src: &FakeSource, sha: &str) -> LoadOutcome {
    load_commit(pool, src, &repo(), sha, None, &owners(), today())
        .await
        .unwrap()
}

/// Bootstrap `files` as the first snapshot (pushed by the configured owner).
async fn bootstrap(pool: &SqlitePool, src: &FakeSource, files: CraFiles, sha_: &str) -> i64 {
    src.commit(sha_, Ok(files), Ok(direct("test-owner")));
    match load(pool, src, sha_).await {
        LoadOutcome::Activated { snapshot_id, .. } => snapshot_id,
        other => panic!("bootstrap should activate, got {other:?}"),
    }
}

fn rejected_codes(o: &LoadOutcome) -> Vec<String> {
    match o {
        LoadOutcome::Rejected { violations, .. } => {
            violations.iter().map(|v| v.code.clone()).collect()
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
}

async fn active_id(pool: &SqlitePool) -> Option<i64> {
    store::active(pool, ORG_ID).await.unwrap().map(|r| r.id)
}

async fn row_count(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM cra_snapshots")
        .fetch_one(pool)
        .await
        .unwrap()
}

// ------------------------------------------------------------- the loader

#[tokio::test]
async fn the_first_snapshot_loads_only_for_an_owner() {
    // A Lead in the new snapshot is nobody before a snapshot exists.
    let pool = pool().await;
    let src = FakeSource::new();
    src.commit(&sha(1), Ok(base()), Ok(direct("lena-gh")));
    let o = load(&pool, &src, &sha(1)).await;
    assert_eq!(rejected_codes(&o), vec!["NotBootstrapOwner"]);
    assert_eq!(active_id(&pool).await, None);
    // Even with an approving review by the owner.
    src.commit(
        &sha(2),
        Ok(base()),
        Ok(via_pr("lena-gh", "lena-gh", &["test-owner"])),
    );
    assert_eq!(
        rejected_codes(&load(&pool, &src, &sha(2)).await),
        vec!["NotBootstrapOwner"]
    );
    assert_eq!(active_id(&pool).await, None);

    // The owner from engine configuration loads it.
    let id = bootstrap(&pool, &src, base(), &sha(3)).await;
    assert_eq!(active_id(&pool).await, Some(id));
    let row = store::get(&pool, id).await.unwrap().unwrap();
    assert_eq!(
        (row.status.as_str(), row.actor.as_deref()),
        ("active", Some("github:test-owner"))
    );
    assert_eq!(row.org_slug.as_deref(), Some("test-org"));
    assert_eq!(row.files.as_ref(), Some(&base()));
}

#[tokio::test]
async fn an_unauthorized_change_is_recorded_as_rejected_and_the_active_snapshot_is_unchanged() {
    let pool = pool().await;
    let src = FakeSource::new();
    let first = bootstrap(&pool, &src, base(), &sha(1)).await;

    // cal holds no position: granting reviewer (rank 40) needs rank above 40.
    let change = add_grant(base(), "cal", "reviewer", "mia", None);
    src.commit(&sha(2), Ok(change.clone()), Ok(direct("cal-gh")));
    let o = load(&pool, &src, &sha(2)).await;
    let codes = rejected_codes(&o);
    assert!(
        codes.contains(&"SelfChange".to_string())
            && codes.contains(&"InsufficientRank".to_string()),
        "{codes:?}"
    );
    assert_eq!(
        active_id(&pool).await,
        Some(first),
        "the active snapshot must not change"
    );

    let LoadOutcome::Rejected {
        snapshot_id,
        retryable,
        ..
    } = o
    else {
        unreachable!()
    };
    assert!(!retryable);
    let row = store::get(&pool, snapshot_id).await.unwrap().unwrap();
    assert_eq!(row.status, "rejected");
    assert_eq!(
        row.files.as_ref(),
        Some(&change),
        "a rejected attempt keeps the files it was judged on"
    );
    assert!(row.plan_markdown.is_some());
    assert_eq!(row.actor.as_deref(), Some("github:cal-gh"));

    // An unknown actor is refused too.
    src.commit(
        &sha(3),
        Ok(add_grant(base(), "cal", "reviewer", "mia", None)),
        Ok(direct("stranger")),
    );
    assert_eq!(
        rejected_codes(&load(&pool, &src, &sha(3)).await),
        vec!["UnknownActor"]
    );
    assert_eq!(active_id(&pool).await, Some(first));

    // Control: the same change by a Maintainer is accepted.
    src.commit(
        &sha(4),
        Ok(add_grant(base(), "cal", "reviewer", "mia", None)),
        Ok(direct("mia-gh")),
    );
    assert!(matches!(
        load(&pool, &src, &sha(4)).await,
        LoadOutcome::Activated { .. }
    ));
}

#[tokio::test]
async fn a_two_person_change_through_a_direct_push_is_rejected() {
    let pool = pool().await;
    let src = FakeSource::new();
    let first = bootstrap(&pool, &src, base(), &sha(1)).await;
    // Maintainer (60) is at the open-mode two-person threshold (60).
    let change = add_grant(base(), "cal", "maintainer", "lena", Some("2027-01-01"));
    src.commit(&sha(2), Ok(change.clone()), Ok(direct("lena-gh")));
    assert_eq!(
        rejected_codes(&load(&pool, &src, &sha(2)).await),
        vec!["DirectPushNeedsApproval"]
    );
    assert_eq!(active_id(&pool).await, Some(first));

    // Control: the same change through a merged pull request approved by a second Lead.
    src.commit(
        &sha(3),
        Ok(change),
        Ok(via_pr("lena-gh", "lena-gh", &["lars-gh"])),
    );
    assert!(matches!(
        load(&pool, &src, &sha(3)).await,
        LoadOutcome::Activated { .. }
    ));
}

#[tokio::test]
async fn a_foundry_api_error_rejects_the_load_and_can_be_retried() {
    let pool = pool().await;
    let src = FakeSource::new();
    let first = bootstrap(&pool, &src, base(), &sha(1)).await;
    let change = add_grant(base(), "cal", "reviewer", "mia", None);

    src.commit(
        &sha(2),
        Err(SourceError::Api("HTTP 502".into())),
        Ok(direct("mia-gh")),
    );
    let o = load(&pool, &src, &sha(2)).await;
    assert_eq!(rejected_codes(&o), vec!["SourceError"]);
    assert!(matches!(
        o,
        LoadOutcome::Rejected {
            retryable: true,
            ..
        }
    ));
    assert_eq!(active_id(&pool).await, Some(first));

    // Files fine, context call fails: still rejected, never "skip the check".
    src.commit(
        &sha(2),
        Ok(change.clone()),
        Err(SourceError::Ambiguous("two merged pulls".into())),
    );
    assert_eq!(
        rejected_codes(&load(&pool, &src, &sha(2)).await),
        vec!["SourceError"]
    );
    assert_eq!(active_id(&pool).await, Some(first));

    // A transient failure is not final: the Foundry API recovers and the commit loads.
    src.commit(&sha(2), Ok(change), Ok(direct("mia-gh")));
    assert!(matches!(
        load(&pool, &src, &sha(2)).await,
        LoadOutcome::Activated { .. }
    ));
    let rejected: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM cra_snapshots WHERE status = 'rejected' AND commit_sha = ?",
    )
    .bind(sha(2))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rejected, 2, "rejected attempts are kept");
}

#[tokio::test]
async fn loading_the_same_commit_twice_does_nothing_the_second_time() {
    let pool = pool().await;
    let src = FakeSource::new();
    let first = bootstrap(&pool, &src, base(), &sha(1)).await;
    assert_eq!(src.fetched().len(), 1);
    let o = load(&pool, &src, &sha(1)).await;
    assert_eq!(
        o,
        LoadOutcome::AlreadyProcessed {
            snapshot_id: first,
            commit_sha: sha(1),
            status: "active".into()
        }
    );
    assert_eq!(
        src.fetched().len(),
        1,
        "a processed commit is not even fetched again"
    );
    assert_eq!(row_count(&pool).await, 1);

    // A rejected commit stays rejected: a redelivery does not get a second verdict.
    src.commit(
        &sha(2),
        Ok(add_grant(base(), "cal", "reviewer", "mia", None)),
        Ok(direct("cal-gh")),
    );
    let LoadOutcome::Rejected { snapshot_id, .. } = load(&pool, &src, &sha(2)).await else {
        panic!()
    };
    // Even if the Foundry API now tells a different story about who pushed it.
    src.commit(
        &sha(2),
        Ok(add_grant(base(), "cal", "reviewer", "mia", None)),
        Ok(direct("mia-gh")),
    );
    assert!(
        matches!(load(&pool, &src, &sha(2)).await, LoadOutcome::AlreadyProcessed { snapshot_id: s, .. } if s == snapshot_id)
    );
    assert_eq!(row_count(&pool).await, 2);

    // An old commit replayed after a newer one cannot roll the hierarchy back.
    src.commit(
        &sha(3),
        Ok(add_grant(base(), "cal", "reviewer", "mia", None)),
        Ok(direct("mia-gh")),
    );
    let LoadOutcome::Activated {
        snapshot_id: third, ..
    } = load(&pool, &src, &sha(3)).await
    else {
        panic!()
    };
    assert!(matches!(
        load(&pool, &src, &sha(1)).await,
        LoadOutcome::AlreadyProcessed { .. }
    ));
    assert_eq!(active_id(&pool).await, Some(third));
}

#[tokio::test]
async fn there_is_never_more_than_one_active_snapshot() {
    let pool = pool().await;
    let src = FakeSource::new();
    bootstrap(&pool, &src, base(), &sha(1)).await;
    src.commit(
        &sha(2),
        Ok(add_grant(base(), "cal", "reviewer", "mia", None)),
        Ok(direct("mia-gh")),
    );
    load(&pool, &src, &sha(2)).await;
    let third = add_grant(
        add_grant(base(), "cal", "reviewer", "mia", None),
        "max",
        "reviewer",
        "lena",
        None,
    );
    src.commit(&sha(3), Ok(third), Ok(direct("lena-gh")));
    let LoadOutcome::Activated { snapshot_id, .. } = load(&pool, &src, &sha(3)).await else {
        panic!()
    };
    assert_eq!(store::active_count(&pool, ORG_ID).await.unwrap(), 1);
    let statuses: Vec<String> = sqlx::query_scalar("SELECT status FROM cra_snapshots ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(statuses, vec!["superseded", "superseded", "active"]);
    assert_eq!(active_id(&pool).await, Some(snapshot_id));

    // The database itself refuses a second active row.
    let r = sqlx::query("INSERT INTO cra_snapshots (org_id, provider, host, repo, commit_sha, status) VALUES (?, 'github', 'github.com', 'x/y', ?, 'active')")
        .bind(ORG_ID)
        .bind(sha(99))
        .execute(&pool)
        .await;
    assert!(
        r.is_err(),
        "the partial unique index must refuse a second active snapshot"
    );

    // An activation planned against a snapshot that is no longer active changes nothing.
    let files = add_grant(base(), "rex", "reviewer", "mia", None);
    let a = Attempt {
        org_id: ORG_ID,
        org_slug: Some("test-org"),
        repo: &repo(),
        commit_sha: &sha(4),
        files: Some(&files),
        ctx: None,
        violations: &[],
        plan_markdown: None,
        retryable: false,
    };
    let stale = store::activate(&pool, &a, Some(1)).await.unwrap();
    assert_eq!(
        stale,
        Activation::ActiveChanged {
            current: Some(snapshot_id)
        }
    );
    assert_eq!(active_id(&pool).await, Some(snapshot_id));
    assert_eq!(row_count(&pool).await, 3);
}

#[tokio::test]
async fn a_commit_that_changes_nothing_is_a_no_op() {
    let pool = pool().await;
    let src = FakeSource::new();
    let first = bootstrap(&pool, &src, base(), &sha(1)).await;
    // Same content (a README-only commit): no plan, nothing recorded.
    src.commit(&sha(2), Ok(base()), Ok(direct("cal-gh")));
    assert_eq!(
        load(&pool, &src, &sha(2)).await,
        LoadOutcome::NoChange { commit_sha: sha(2) }
    );
    assert_eq!(row_count(&pool).await, 1);
    assert_eq!(active_id(&pool).await, Some(first));
}

#[tokio::test]
async fn invalid_files_and_bad_shas_are_rejected() {
    let pool = pool().await;
    let src = FakeSource::new();
    let first = bootstrap(&pool, &src, base(), &sha(1)).await;
    let bad = edit(base(), F::Roles, "rank = 80", "rank = 1000");
    src.commit(&sha(2), Ok(bad), Ok(direct("test-owner")));
    assert!(
        rejected_codes(&load(&pool, &src, &sha(2)).await).contains(&"RankOutOfRange".to_string())
    );
    assert_eq!(active_id(&pool).await, Some(first));
    // A branch name is never a commit.
    for not_a_sha in ["main", "refs/heads/main", "abc123", &"0".repeat(40)] {
        assert_eq!(
            rejected_codes(&load(&pool, &src, not_a_sha).await),
            vec!["InvalidCommitSha"],
            "{not_a_sha}"
        );
    }
    assert!(
        !src.fetched().iter().any(|s| s == "main"),
        "a branch name must never reach the Foundry API"
    );
    assert_eq!(active_id(&pool).await, Some(first));
}

#[tokio::test]
async fn a_corrupt_active_row_fails_closed() {
    let pool = pool().await;
    let src = FakeSource::new();
    let first = bootstrap(&pool, &src, base(), &sha(1)).await;
    sqlx::query("UPDATE cra_snapshots SET roles_toml = roles_toml || '\n# tampered' WHERE id = ?")
        .bind(first)
        .execute(&pool)
        .await
        .unwrap();
    src.commit(
        &sha(2),
        Ok(add_grant(base(), "cal", "reviewer", "mia", None)),
        Ok(direct("mia-gh")),
    );
    assert_eq!(
        rejected_codes(&load(&pool, &src, &sha(2)).await),
        vec!["ActiveSnapshotUnusable"]
    );
    assert_eq!(active_id(&pool).await, Some(first));
    let h = CraHandle::new();
    assert!(
        h.reload_from_store(&pool, ORG_ID).await.is_err(),
        "startup must refuse an unusable active snapshot"
    );
    // And the payout policy refuses to fall back to anything else.
    let cfg = PaymentConfig::from_lookup(|_| None);
    sqlx::query("UPDATE cra_snapshots SET payouts_toml = 'nonsense =' WHERE id = ?")
        .bind(first)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        pay_policy::load_effective(&pool, DEFAULT_ORG, &cfg).await,
        Err(pay_policy::PolicyError::Corrupt(_))
    ));
}

// ---------------------------------------------------------- the trigger

fn ctx_with(pool: &SqlitePool, src: Arc<FakeSource>, branch: &str) -> CraContext {
    let handle = CraHandle::new();
    let dir: Arc<dyn Directory> = Arc::new(
        CraDirectory::new(EnvDirectory::new(owners()), handle.clone()).with_today(today()),
    );
    CraContext::new(
        pool.clone(),
        handle,
        dir,
        owners(),
        Some(CraSettings {
            repo: repo(),
            branch: branch.into(),
        }),
        Some(src as Arc<dyn CraSource>),
    )
}

fn gh_push(repo_name: &str, branch_ref: &str, after: &str, sender: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "ref": branch_ref, "after": after, "deleted": false,
        "repository": {"full_name": repo_name, "default_branch": "main"},
        "sender": {"login": sender}, "pusher": {"name": sender},
        // A forged copy of the files in the body is never read.
        "msync": {"members.toml": "schema = 1\n[[member]]\nid = \"evil\"\nname = \"Evil\"\nstatus = \"active\"\n"},
        "commits": [{"id": after, "modified": [".msync/members.toml"]}]
    }))
    .unwrap()
}

#[tokio::test]
async fn a_push_to_another_repository_or_branch_loads_nothing() {
    let pool = pool().await;
    let src = Arc::new(FakeSource::new());
    src.commit(&sha(1), Ok(base()), Ok(direct("test-owner")));
    let ctx = ctx_with(&pool, src.clone(), "main");

    for body in [
        gh_push("example/other", "refs/heads/main", &sha(1), "test-owner"), // another repository
        gh_push("example/cra", "refs/heads/feature", &sha(1), "test-owner"), // another branch
        gh_push("example/cra", "refs/tags/main", &sha(1), "test-owner"), // a tag named like the branch
        gh_push(
            "other/example/cra",
            "refs/heads/main",
            &sha(1),
            "test-owner",
        ),
    ] {
        assert!(crate::routes::github_cra_push(&ctx, "push", &body, None)
            .await
            .is_none());
    }
    // Not a push event at all.
    let body = gh_push("example/cra", "refs/heads/main", &sha(1), "test-owner");
    assert!(
        crate::routes::github_cra_push(&ctx, "pull_request", &body, None)
            .await
            .is_none()
    );
    assert!(
        src.fetched().is_empty(),
        "nothing may be fetched for a non-CRA push"
    );
    assert_eq!(row_count(&pool).await, 0);
    assert!(ctx.handle.active().is_none());

    // Control: the CRA repository's branch (case-insensitive repo path) loads and swaps the handle.
    let body = gh_push("Example/CRA", "refs/heads/main", &sha(1), "test-owner");
    let r = crate::routes::github_cra_push(&ctx, "push", &body, None)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(r, LoadOutcome::Activated { .. }), "{r:?}");
    assert_eq!(
        src.fetched(),
        vec![sha(1)],
        "the files are fetched at the pushed SHA"
    );
    let active = ctx
        .handle
        .active()
        .expect("the handle holds the new snapshot");
    assert_eq!(active.commit_sha, sha(1));
    assert!(
        active.snapshot.member("evil").is_none(),
        "the webhook body is never trusted for content"
    );
    assert_eq!(
        active.snapshot.members,
        crate::cra::load(&base(), today()).unwrap().members
    );
}

#[tokio::test]
async fn a_gitlab_push_hook_triggers_only_for_the_cra_repository() {
    use actix_web::test::TestRequest;
    let pool = pool().await;
    let gl_repo = CraRepo::new(FoundryProvider::GitLab, "gitlab.com", "grp/cra");
    let src = Arc::new(FakeSource {
        repo: gl_repo.clone(),
        ..FakeSource::new()
    });
    let gl_owner = ChangeContext {
        actor: FoundryIdentity::gitlab("test-owner"),
        ..direct("x")
    };
    src.commit(&sha(7), Ok(base()), Ok(gl_owner));
    let handle = CraHandle::new();
    let dir: Arc<dyn Directory> = Arc::new(EnvDirectory::default());
    let ctx = CraContext::new(
        pool.clone(),
        handle,
        dir,
        vec!["gitlab:test-owner".into()],
        Some(CraSettings {
            repo: gl_repo,
            branch: "main".into(),
        }),
        Some(src.clone() as Arc<dyn CraSource>),
    );
    let headers = TestRequest::post()
        .insert_header(("X-Gitlab-Token", "s"))
        .insert_header(("X-Gitlab-Event", "Push Hook"))
        .to_http_request()
        .headers()
        .clone();
    let reg = Arc::new(RepoRegistry::from_spec("gitlab:gitlab.com:grp/cra").unwrap());
    let pay = Arc::new(PaymentConfig::from_lookup(|_| None));
    let rules = Arc::new(MeritRules::default());
    let call = |body: serde_json::Value| {
        let (ctx, headers, reg, pay, rules, pool) = (
            &ctx,
            headers.clone(),
            reg.clone(),
            pay.clone(),
            rules.clone(),
            pool.clone(),
        );
        async move {
            let b = serde_json::to_vec(&body).unwrap();
            crate::routes::process_gitlab_webhook_with_cra(
                &pool,
                Some("s"),
                reg,
                pay,
                rules,
                Some(ctx),
                &headers,
                &b,
            )
            .await
        }
    };
    use crate::foundry::gitlab::tests::push_hook;
    let (st, j) = call(push_hook(
        "refs/heads/dev",
        &sha(7),
        "test-owner",
        "grp/cra",
        "https://gitlab.com/grp/cra",
    ))
    .await;
    assert_eq!((st.as_u16(), j["status"].as_str()), (200, Some("ignored")));
    let (_, j) = call(push_hook(
        "refs/heads/main",
        &sha(7),
        "test-owner",
        "grp/other",
        "https://gitlab.com/grp/other",
    ))
    .await;
    assert_eq!(j["status"], "ignored");
    let (_, j) = call(push_hook(
        "refs/heads/main",
        &sha(7),
        "test-owner",
        "grp/cra",
        "https://gitlab.example.org/grp/cra",
    ))
    .await;
    assert_eq!(
        j["status"], "ignored",
        "same path on another host is another repository"
    );
    assert!(src.fetched().is_empty());
    // A wrong token is refused before anything else.
    let bad = TestRequest::post()
        .insert_header(("X-Gitlab-Token", "nope"))
        .to_http_request()
        .headers()
        .clone();
    let b = serde_json::to_vec(&push_hook(
        "refs/heads/main",
        &sha(7),
        "test-owner",
        "grp/cra",
        "https://gitlab.com/grp/cra",
    ))
    .unwrap();
    let (st, _) = crate::routes::process_gitlab_webhook_with_cra(
        &pool,
        Some("s"),
        reg.clone(),
        pay.clone(),
        rules.clone(),
        Some(&ctx),
        &bad,
        &b,
    )
    .await;
    assert_eq!(st.as_u16(), 401);
    assert!(src.fetched().is_empty());

    let (st, j) = call(push_hook(
        "refs/heads/main",
        &sha(7),
        "test-owner",
        "grp/cra",
        "https://gitlab.com/grp/cra",
    ))
    .await;
    assert_eq!(
        (st.as_u16(), j["status"].as_str()),
        (200, Some("activated")),
        "{j}"
    );
    assert_eq!(src.fetched(), vec![sha(7)]);
    assert!(ctx.handle.active().is_some());
}

// ---------------------------------------------------- GitHub against mockito

const GH_PATH: &str = "example/cra";

async fn gh_files(
    server: &mut mockito::ServerGuard,
    sha_: &str,
    files: &CraFiles,
) -> Vec<mockito::Mock> {
    let mut mocks = Vec::new();
    for (kind, text) in files.ordered() {
        mocks.push(
            server
                .mock(
                    "GET",
                    format!("/repos/{GH_PATH}/contents/.msync/{}", kind.file_name()).as_str(),
                )
                .match_query(Matcher::UrlEncoded("ref".into(), sha_.into()))
                .match_header("accept", "application/vnd.github.raw+json")
                .with_status(200)
                .with_body(text)
                .create_async()
                .await,
        );
    }
    mocks
}

fn gh_source(server: &mockito::ServerGuard) -> GitHubSource {
    GitHubSource::new(repo(), Some(server.url()), Some("test-token".into()))
}

#[tokio::test]
async fn github_files_are_fetched_at_the_pushed_sha() {
    let mut server = mockito::Server::new_async().await;
    let pool = pool().await;
    let s = sha(0xabc);
    let file_mocks = gh_files(&mut server, &s, &base()).await;
    let pulls = server
        .mock(
            "GET",
            format!("/repos/{GH_PATH}/commits/{s}/pulls").as_str(),
        )
        .match_query(Matcher::Any)
        .with_status(200)
        .with_body("[]")
        .create_async()
        .await;
    let src = gh_source(&server);
    let o = load_commit(
        &pool,
        &src,
        &repo(),
        &s,
        Some(&gh("test-owner")),
        &owners(),
        today(),
    )
    .await
    .unwrap();
    assert!(matches!(o, LoadOutcome::Activated { .. }), "{o:?}");
    for m in &file_mocks {
        m.assert_async().await;
    }
    pulls.assert_async().await;
    let row = store::active(&pool, ORG_ID).await.unwrap().unwrap();
    assert_eq!(row.files.as_ref(), Some(&base()));
    assert_eq!(row.commit_sha, s);
}

#[tokio::test]
async fn github_api_errors_reject_the_load() {
    let mut server = mockito::Server::new_async().await;
    let pool = pool().await;
    let s = sha(0xdef);
    gh_files(&mut server, &s, &base()).await;
    server
        .mock(
            "GET",
            format!("/repos/{GH_PATH}/commits/{s}/pulls").as_str(),
        )
        .match_query(Matcher::Any)
        .with_status(500)
        .create_async()
        .await;
    let src = gh_source(&server);
    let o = load_commit(
        &pool,
        &src,
        &repo(),
        &s,
        Some(&gh("test-owner")),
        &owners(),
        today(),
    )
    .await
    .unwrap();
    assert_eq!(rejected_codes(&o), vec!["SourceError"]);
    assert_eq!(active_id(&pool).await, None);

    // A missing file is an error too, never "use what we have".
    let s2 = sha(0xdee);
    server
        .mock(
            "GET",
            format!("/repos/{GH_PATH}/contents/.msync/org.toml").as_str(),
        )
        .match_query(Matcher::UrlEncoded("ref".into(), s2.clone()))
        .with_status(404)
        .create_async()
        .await;
    let o = load_commit(
        &pool,
        &src,
        &repo(),
        &s2,
        Some(&gh("test-owner")),
        &owners(),
        today(),
    )
    .await
    .unwrap();
    assert_eq!(rejected_codes(&o), vec!["SourceError"]);
    assert_eq!(active_id(&pool).await, None);
}

/// Mocks a merged pull request #12 for commit `merge`, head `head`, with `reviews`.
async fn gh_merged_pr(
    server: &mut mockito::ServerGuard,
    merge: &str,
    head: &str,
    merger: &str,
    author: &str,
    reviews: serde_json::Value,
) {
    server
        .mock(
            "GET",
            format!("/repos/{GH_PATH}/commits/{merge}/pulls").as_str(),
        )
        .match_query(Matcher::Any)
        .with_status(200)
        .with_body(
            json!([{"number": 12, "merged_at": "2026-11-12T10:00:00Z", "merge_commit_sha": merge}])
                .to_string(),
        )
        .create_async()
        .await;
    server
        .mock("GET", format!("/repos/{GH_PATH}/pulls/12").as_str())
        .with_status(200)
        .with_body(
            json!({"number": 12, "merged": true, "merge_commit_sha": merge, "merged_by": {"login": merger},
                   "user": {"login": author}, "head": {"sha": head}})
            .to_string(),
        )
        .create_async()
        .await;
    server
        .mock("GET", format!("/repos/{GH_PATH}/pulls/12/reviews").as_str())
        .match_query(Matcher::Any)
        .with_status(200)
        .with_body(reviews.to_string())
        .create_async()
        .await;
}

#[tokio::test]
async fn github_an_approval_that_predates_the_last_commit_does_not_count() {
    let pool = pool().await;
    let fake = FakeSource::new();
    bootstrap(&pool, &fake, base(), &sha(1)).await;
    let change = add_grant(base(), "cal", "maintainer", "lena", Some("2027-01-01")); // two-person
    let (head, old_head) = (sha(0x5ead), sha(0x01d));

    // lars approved an earlier commit of the pull request; lena pushed again, then merged.
    let mut server = mockito::Server::new_async().await;
    let merge = sha(0x111);
    gh_files(&mut server, &merge, &change).await;
    gh_merged_pr(
        &mut server,
        &merge,
        &head,
        "lena-gh",
        "lena-gh",
        json!([{"user": {"login": "lars-gh"}, "state": "APPROVED", "commit_id": old_head}]),
    )
    .await;
    let o = load_commit(
        &pool,
        &gh_source(&server),
        &repo(),
        &merge,
        Some(&gh("lena-gh")),
        &owners(),
        today(),
    )
    .await
    .unwrap();
    assert_eq!(rejected_codes(&o), vec!["NoQualifiedApprover"]);

    // A dismissed approval on the head does not count either.
    let mut server = mockito::Server::new_async().await;
    let merge = sha(0x222);
    gh_files(&mut server, &merge, &change).await;
    gh_merged_pr(
        &mut server,
        &merge,
        &head,
        "lena-gh",
        "lena-gh",
        json!([
            {"user": {"login": "lars-gh"}, "state": "APPROVED", "commit_id": head},
            {"user": {"login": "lars-gh"}, "state": "DISMISSED", "commit_id": head}
        ]),
    )
    .await;
    let o = load_commit(
        &pool,
        &gh_source(&server),
        &repo(),
        &merge,
        Some(&gh("lena-gh")),
        &owners(),
        today(),
    )
    .await
    .unwrap();
    assert_eq!(rejected_codes(&o), vec!["NoQualifiedApprover"]);

    // Control: an approval of the head commit counts and the change activates.
    let mut server = mockito::Server::new_async().await;
    let merge = sha(0x333);
    gh_files(&mut server, &merge, &change).await;
    gh_merged_pr(
        &mut server,
        &merge,
        &head,
        "lena-gh",
        "lena-gh",
        json!([{"user": {"login": "lars-gh"}, "state": "APPROVED", "commit_id": head}]),
    )
    .await;
    let o = load_commit(
        &pool,
        &gh_source(&server),
        &repo(),
        &merge,
        Some(&gh("lena-gh")),
        &owners(),
        today(),
    )
    .await
    .unwrap();
    assert!(matches!(o, LoadOutcome::Activated { .. }), "{o:?}");
    let row = store::active(&pool, ORG_ID).await.unwrap().unwrap();
    assert!(row.via_pull_request);
    assert_eq!(row.approvers, vec!["github:lars-gh"]);
    assert_eq!(row.author.as_deref(), Some("github:lena-gh"));
}

#[tokio::test]
async fn github_only_a_pull_request_merged_as_this_commit_counts() {
    let pool = pool().await;
    let fake = FakeSource::new();
    bootstrap(&pool, &fake, base(), &sha(1)).await;
    let change = add_grant(base(), "cal", "maintainer", "lena", Some("2027-01-01"));
    let mut server = mockito::Server::new_async().await;
    let pushed = sha(0x444);
    gh_files(&mut server, &pushed, &change).await;
    // The commit belongs to an approved pull request, but that request was merged as another
    // commit (or not at all): this push is a direct push.
    server
        .mock(
            "GET",
            format!("/repos/{GH_PATH}/commits/{pushed}/pulls").as_str(),
        )
        .match_query(Matcher::Any)
        .with_status(200)
        .with_body(
            json!([
                {"number": 12, "merged_at": "2026-11-12T10:00:00Z", "merge_commit_sha": sha(0x999)},
                {"number": 13, "merged_at": null, "merge_commit_sha": pushed}
            ])
            .to_string(),
        )
        .create_async()
        .await;
    let o = load_commit(
        &pool,
        &gh_source(&server),
        &repo(),
        &pushed,
        Some(&gh("lena-gh")),
        &owners(),
        today(),
    )
    .await
    .unwrap();
    assert_eq!(rejected_codes(&o), vec!["DirectPushNeedsApproval"]);

    // A webhook pusher who is not the merger is an ambiguous answer: refused.
    let mut server = mockito::Server::new_async().await;
    let merge = sha(0x555);
    gh_files(&mut server, &merge, &change).await;
    gh_merged_pr(
        &mut server,
        &merge,
        &sha(0x5ead),
        "lena-gh",
        "lena-gh",
        json!([{"user": {"login": "lars-gh"}, "state": "APPROVED", "commit_id": sha(0x5ead)}]),
    )
    .await;
    let o = load_commit(
        &pool,
        &gh_source(&server),
        &repo(),
        &merge,
        Some(&gh("mia-gh")),
        &owners(),
        today(),
    )
    .await
    .unwrap();
    assert_eq!(rejected_codes(&o), vec!["SourceError"]);
}

#[tokio::test]
async fn github_a_direct_push_without_a_verified_pusher_is_refused() {
    // A reload of a directly pushed commit: the API cannot say who pushed it.
    let mut server = mockito::Server::new_async().await;
    let pool = pool().await;
    let s = sha(0x777);
    gh_files(&mut server, &s, &base()).await;
    server
        .mock(
            "GET",
            format!("/repos/{GH_PATH}/commits/{s}/pulls").as_str(),
        )
        .match_query(Matcher::Any)
        .with_status(200)
        .with_body("[]")
        .create_async()
        .await;
    let o = load_commit(
        &pool,
        &gh_source(&server),
        &repo(),
        &s,
        None,
        &owners(),
        today(),
    )
    .await
    .unwrap();
    assert_eq!(rejected_codes(&o), vec!["SourceError"]);
    assert_eq!(active_id(&pool).await, None);
}

#[test]
fn github_review_verdicts() {
    let h = "aaaa";
    let r = |login: &str, state: &str, commit: Option<&str>| json!({"user": {"login": login}, "state": state, "commit_id": commit});
    let reviews = vec![
        r("a", "APPROVED", Some(h)),
        r("b", "APPROVED", Some("old")),
        r("c", "APPROVED", Some(h)),
        r("c", "CHANGES_REQUESTED", Some(h)),
        r("d", "CHANGES_REQUESTED", Some(h)),
        r("d", "APPROVED", Some(h)),
        r("d", "COMMENTED", Some(h)),
        r("e", "APPROVED", None),
        r("f", "DISMISSED", Some(h)),
    ];
    assert_eq!(github_approvers_sorted(&reviews, h), vec!["a", "d"]);
}

fn github_approvers_sorted(reviews: &[serde_json::Value], head: &str) -> Vec<String> {
    let mut v = crate::cra::source::github_approvers(reviews, head);
    v.sort();
    v
}

// ---------------------------------------------------- GitLab against mockito

const GL_ENC: &str = "grp%2Fcra";

fn gl_repo() -> CraRepo {
    CraRepo::new(FoundryProvider::GitLab, "gitlab.com", "grp/cra")
}

async fn gl_files(
    server: &mut mockito::ServerGuard,
    sha_: &str,
    files: &CraFiles,
) -> Vec<mockito::Mock> {
    let mut mocks = Vec::new();
    for (kind, text) in files.ordered() {
        mocks.push(
            server
                .mock(
                    "GET",
                    format!(
                        "/projects/{GL_ENC}/repository/files/.msync%2F{}/raw",
                        kind.file_name()
                    )
                    .as_str(),
                )
                .match_query(Matcher::UrlEncoded("ref".into(), sha_.into()))
                .match_header("private-token", "gl-token")
                .with_status(200)
                .with_body(text)
                .create_async()
                .await,
        );
    }
    mocks
}

fn gl_source(server: &mockito::ServerGuard) -> GitLabSource {
    GitLabSource::new(gl_repo(), Some(server.url()), Some("gl-token".into()))
}

/// GitLab flavour of the base fixture: every member's identity is on GitLab.
fn gl_base() -> CraFiles {
    let mut f = base();
    f.members = f.members.replace("github = ", "gitlab = ");
    f.org = f
        .org
        .replace("provider = \"github\"", "provider = \"gitlab\"");
    f
}

fn gl_owners() -> Vec<String> {
    vec!["gitlab:test-owner".to_string()]
}

#[tokio::test]
async fn gitlab_files_are_fetched_at_the_pushed_sha() {
    let mut server = mockito::Server::new_async().await;
    let pool = pool().await;
    let s = sha(0xa1);
    let file_mocks = gl_files(&mut server, &s, &gl_base()).await;
    let mrs = server
        .mock(
            "GET",
            format!("/projects/{GL_ENC}/repository/commits/{s}/merge_requests").as_str(),
        )
        .with_status(200)
        .with_body("[]")
        .create_async()
        .await;
    let o = load_commit(
        &pool,
        &gl_source(&server),
        &gl_repo(),
        &s,
        Some(&FoundryIdentity::gitlab("test-owner")),
        &gl_owners(),
        today(),
    )
    .await
    .unwrap();
    assert!(matches!(o, LoadOutcome::Activated { .. }), "{o:?}");
    for m in &file_mocks {
        m.assert_async().await;
    }
    mrs.assert_async().await;
    assert_eq!(
        store::active(&pool, ORG_ID).await.unwrap().unwrap().files,
        Some(gl_base())
    );
}

async fn gl_merged_mr(
    server: &mut mockito::ServerGuard,
    merge: &str,
    merger: &str,
    author: &str,
    approved_by: &[&str],
    notes: serde_json::Value,
    last_push: &str,
) {
    server
        .mock(
            "GET",
            format!("/projects/{GL_ENC}/repository/commits/{merge}/merge_requests").as_str(),
        )
        .with_status(200)
        .with_body(json!([{"iid": 4, "state": "merged", "merge_commit_sha": merge}]).to_string())
        .create_async()
        .await;
    server
        .mock(
            "GET",
            format!("/projects/{GL_ENC}/merge_requests/4").as_str(),
        )
        .with_status(200)
        .with_body(
            json!({"iid": 4, "state": "merged", "merge_commit_sha": merge,
            "merge_user": {"username": merger}, "author": {"username": author}})
            .to_string(),
        )
        .create_async()
        .await;
    let approved: Vec<serde_json::Value> = approved_by
        .iter()
        .map(|u| json!({"user": {"username": u}}))
        .collect();
    server
        .mock(
            "GET",
            format!("/projects/{GL_ENC}/merge_requests/4/approvals").as_str(),
        )
        .with_status(200)
        .with_body(json!({"approved_by": approved}).to_string())
        .create_async()
        .await;
    server
        .mock("GET", format!("/projects/{GL_ENC}/merge_requests/4/versions").as_str())
        .match_query(Matcher::Any)
        .with_status(200)
        .with_body(json!([{"id": 2, "created_at": last_push}, {"id": 1, "created_at": "2026-11-01T00:00:00Z"}]).to_string())
        .create_async()
        .await;
    server
        .mock(
            "GET",
            format!("/projects/{GL_ENC}/merge_requests/4/notes").as_str(),
        )
        .match_query(Matcher::Any)
        .with_status(200)
        .with_body(notes.to_string())
        .create_async()
        .await;
}

fn note(user: &str, body: &str, at: &str) -> serde_json::Value {
    json!({"system": true, "author": {"username": user}, "body": body, "created_at": at})
}

#[tokio::test]
async fn gitlab_an_approval_that_predates_the_last_commit_does_not_count() {
    let pool = pool().await;
    let fake = FakeSource {
        repo: gl_repo(),
        ..FakeSource::new()
    };
    fake.commit(
        &sha(1),
        Ok(gl_base()),
        Ok(ChangeContext {
            actor: FoundryIdentity::gitlab("test-owner"),
            ..direct("x")
        }),
    );
    assert!(matches!(
        load_commit(
            &pool,
            &fake,
            &gl_repo(),
            &sha(1),
            None,
            &gl_owners(),
            today()
        )
        .await
        .unwrap(),
        LoadOutcome::Activated { .. }
    ));
    let change = add_grant(gl_base(), "cal", "maintainer", "lena", Some("2027-01-01"));
    let gl = |l: &str| FoundryIdentity::gitlab(l);

    // lars approved at 10:00, lena pushed again at 11:00 (approvals not reset), then merged.
    let mut server = mockito::Server::new_async().await;
    let merge = sha(0xb1);
    gl_files(&mut server, &merge, &change).await;
    gl_merged_mr(
        &mut server,
        &merge,
        "lena-gh",
        "lena-gh",
        &["lars-gh"],
        json!([note(
            "lars-gh",
            "approved this merge request",
            "2026-11-12T10:00:00Z"
        )]),
        "2026-11-12T11:00:00Z",
    )
    .await;
    let o = load_commit(
        &pool,
        &gl_source(&server),
        &gl_repo(),
        &merge,
        Some(&gl("lena-gh")),
        &gl_owners(),
        today(),
    )
    .await
    .unwrap();
    assert_eq!(rejected_codes(&o), vec!["NoQualifiedApprover"]);

    // Approved after the last push but later revoked: not in approved_by, does not count.
    let mut server = mockito::Server::new_async().await;
    let merge = sha(0xb2);
    gl_files(&mut server, &merge, &change).await;
    gl_merged_mr(
        &mut server,
        &merge,
        "lena-gh",
        "lena-gh",
        &[],
        json!([
            note(
                "lars-gh",
                "approved this merge request",
                "2026-11-12T12:00:00Z"
            ),
            note(
                "lars-gh",
                "unapproved this merge request",
                "2026-11-12T12:30:00Z"
            )
        ]),
        "2026-11-12T11:00:00Z",
    )
    .await;
    let o = load_commit(
        &pool,
        &gl_source(&server),
        &gl_repo(),
        &merge,
        Some(&gl("lena-gh")),
        &gl_owners(),
        today(),
    )
    .await
    .unwrap();
    assert_eq!(rejected_codes(&o), vec!["NoQualifiedApprover"]);

    // Control: approved after the last push, still approved: counts.
    let mut server = mockito::Server::new_async().await;
    let merge = sha(0xb3);
    gl_files(&mut server, &merge, &change).await;
    gl_merged_mr(
        &mut server,
        &merge,
        "lena-gh",
        "lena-gh",
        &["lars-gh"],
        json!([note(
            "lars-gh",
            "approved this merge request",
            "2026-11-12T12:00:00Z"
        )]),
        "2026-11-12T11:00:00Z",
    )
    .await;
    let o = load_commit(
        &pool,
        &gl_source(&server),
        &gl_repo(),
        &merge,
        Some(&gl("lena-gh")),
        &gl_owners(),
        today(),
    )
    .await
    .unwrap();
    assert!(matches!(o, LoadOutcome::Activated { .. }), "{o:?}");
    assert_eq!(
        store::active(&pool, ORG_ID)
            .await
            .unwrap()
            .unwrap()
            .approvers,
        vec!["gitlab:lars-gh"]
    );
}

#[tokio::test]
async fn gitlab_api_errors_reject_the_load() {
    let pool = pool().await;
    let fake = FakeSource {
        repo: gl_repo(),
        ..FakeSource::new()
    };
    fake.commit(
        &sha(1),
        Ok(gl_base()),
        Ok(ChangeContext {
            actor: FoundryIdentity::gitlab("test-owner"),
            ..direct("x")
        }),
    );
    let first = match load_commit(
        &pool,
        &fake,
        &gl_repo(),
        &sha(1),
        None,
        &gl_owners(),
        today(),
    )
    .await
    .unwrap()
    {
        LoadOutcome::Activated { snapshot_id, .. } => snapshot_id,
        o => panic!("{o:?}"),
    };
    let change = add_grant(gl_base(), "cal", "maintainer", "lena", Some("2027-01-01"));
    let mut server = mockito::Server::new_async().await;
    let merge = sha(0xc1);
    gl_files(&mut server, &merge, &change).await;
    server
        .mock(
            "GET",
            format!("/projects/{GL_ENC}/repository/commits/{merge}/merge_requests").as_str(),
        )
        .with_status(200)
        .with_body(json!([{"iid": 4, "state": "merged", "merge_commit_sha": merge}]).to_string())
        .create_async()
        .await;
    server
        .mock(
            "GET",
            format!("/projects/{GL_ENC}/merge_requests/4").as_str(),
        )
        .with_status(200)
        .with_body(
            json!({"iid": 4, "state": "merged", "merge_commit_sha": merge,
            "merge_user": {"username": "lena-gh"}, "author": {"username": "lena-gh"}})
            .to_string(),
        )
        .create_async()
        .await;
    server
        .mock(
            "GET",
            format!("/projects/{GL_ENC}/merge_requests/4/approvals").as_str(),
        )
        .with_status(503)
        .create_async()
        .await;
    let o = load_commit(
        &pool,
        &gl_source(&server),
        &gl_repo(),
        &merge,
        Some(&FoundryIdentity::gitlab("lena-gh")),
        &gl_owners(),
        today(),
    )
    .await
    .unwrap();
    assert_eq!(rejected_codes(&o), vec!["SourceError"]);
    assert_eq!(active_id(&pool).await, Some(first));
}

#[test]
fn gitlab_approval_verdicts_need_every_fact() {
    let approvals =
        json!({"approved_by": [{"user": {"username": "a"}}, {"user": {"username": "b"}}]});
    let versions = vec![json!({"created_at": "2026-11-12T11:00:00Z"})];
    let notes = vec![
        note("a", "approved this merge request", "2026-11-12T12:00:00Z"),
        note("b", "approved this merge request", "2026-11-12T10:59:59Z"),
    ];
    assert_eq!(
        crate::cra::source::gitlab_approvers(&approvals, &notes, &versions),
        vec!["a"]
    );
    // No versions: the last push time is unknown, so nothing counts.
    assert!(crate::cra::source::gitlab_approvers(&approvals, &notes, &[]).is_empty());
    // A non-system note that says the words is not an approval.
    let fake = vec![
        json!({"system": false, "author": {"username": "a"}, "body": "approved this merge request", "created_at": "2026-11-12T12:00:00Z"}),
    ];
    assert!(crate::cra::source::gitlab_approvers(&approvals, &fake, &versions).is_empty());
}

// ------------------------------------------------------------------ seams

fn eustress() -> CraFiles {
    CraFiles {
        org: include_str!("../../examples/cra/eustress-engine/.msync/org.toml").into(),
        roles: include_str!("../../examples/cra/eustress-engine/.msync/roles.toml").into(),
        members: include_str!("../../examples/cra/eustress-engine/.msync/members.toml").into(),
        merit: include_str!("../../examples/cra/eustress-engine/.msync/merit.toml").into(),
        payouts: include_str!("../../examples/cra/eustress-engine/.msync/payouts.toml").into(),
    }
}

fn example(name: &str) -> CraFiles {
    let read = |f: &str| {
        std::fs::read_to_string(format!(
            "{}/examples/cra/{name}/.msync/{f}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    };
    CraFiles {
        org: read("org.toml"),
        roles: read("roles.toml"),
        members: read("members.toml"),
        merit: read("merit.toml"),
        payouts: read("payouts.toml"),
    }
}

const FOUNDER: &str = "github:example-founder";

/// Activate the Eustress example through the loader, owner bootstrap. Returns the
/// directory and handle the engine would use.
async fn eustress_active(pool: &SqlitePool, files: CraFiles) -> (CraHandle, CraDirectory) {
    let src = FakeSource::new();
    src.commit(&sha(1), Ok(files), Ok(direct("example-founder")));
    let o = load_commit(
        pool,
        &src,
        &repo(),
        &sha(1),
        None,
        &[FOUNDER.to_string()],
        today(),
    )
    .await
    .unwrap();
    assert!(matches!(o, LoadOutcome::Activated { .. }), "{o:?}");
    let handle = CraHandle::new();
    handle.reload_from_store(pool, ORG_ID).await.unwrap();
    let dir = CraDirectory::new(EnvDirectory::new(vec![FOUNDER.to_string()]), handle.clone())
        .with_today(today());
    (handle, dir)
}

#[test]
fn the_example_payout_policies_convert_and_validate() {
    for name in ["eustress-engine", "weave-solutions", "voltec-industries"] {
        let snap =
            crate::cra::load(&example(name), today()).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        let p = payout_policy_from_cra(&snap.payouts).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert!(p.validate().is_empty(), "{name}");
        assert_eq!(
            p.approval.approvals_required,
            i64::from(snap.payouts.approval.approvals_required),
            "{name}"
        );
        assert_eq!(
            p.approval.min_approver_rank,
            i64::from(snap.payouts.approval.min_approver_rank),
            "{name}"
        );
        assert_eq!(
            p.limits.hold_days,
            Some(i64::from(snap.payouts.limits.hold_days)),
            "{name}"
        );
        assert_eq!(p.rate.len(), snap.payouts.rates.len(), "{name}");
        assert_eq!(p.enabled, snap.payouts.enabled, "{name}");
        // The JSON form round-trips through the engine's strict deserializer.
        let back: pay_policy::PayoutPolicy =
            serde_json::from_value(serde_json::to_value(&p).unwrap()).unwrap();
        assert_eq!(back, p, "{name}");
    }
    let e =
        payout_policy_from_cra(&crate::cra::load(&eustress(), today()).unwrap().payouts).unwrap();
    assert_eq!(
        (e.approval.approvals_required, e.approval.min_approver_rank),
        (2, 60)
    );
    assert_eq!(
        e.schedule.day,
        Some(pay_policy::ScheduleDay::Weekday("friday".into()))
    );
    assert_eq!(e.amount_for("main", 50), Some((1000, 1500)));
}

#[tokio::test]
async fn with_a_snapshot_rank_and_policy_come_from_the_cra() {
    let pool = pool().await;
    let (handle, dir) = eustress_active(&pool, eustress()).await;
    handle.set_merit_for_test("cy-example", 60); // Contributor tier (rank 10)

    assert_eq!(dir.rank_of("ada-example"), Some(60), "Maintainer");
    assert_eq!(dir.rank_of("mckale"), Some(80), "Lead Maintainer");
    assert_eq!(dir.rank_of("bo-example"), Some(40), "Reviewer");
    assert_eq!(dir.rank_of("cy-example"), Some(10), "Contributor tier");
    assert_eq!(
        dir.rank_of("founder"),
        Some(1000),
        "owner by GitHub or GitLab identity stays 1000"
    );
    assert_eq!(dir.rank_of("nobody"), None);
    assert_eq!(dir.roles_of("ada-example"), vec!["maintainer", "newcomer"]);
    assert_eq!(dir.roles_of("cy-example"), vec!["contributor"]);

    // The policy in force is payouts.toml: two approvers of rank >= 60.
    let cfg = PaymentConfig::from_lookup(|_| None);
    let eff = pay_policy::load_effective(&pool, DEFAULT_ORG, &cfg)
        .await
        .unwrap();
    assert_eq!(eff.policy.approval.min_approver_rank, 60);
    assert_eq!(
        eff.updated_by.as_deref(),
        Some(format!("cra:{}", sha(1)).as_str())
    );

    // A ledger row for a payee, approved by a Maintainer and by a Contributor.
    let e = ledger::record_merge(
        &pool,
        &cfg,
        &ledger::MergeEvent {
            pr_id: 1,
            pr_number: 1,
            merge_sha: "m1".into(),
            repo: Some("example-eustress/eustress-engine".into()),
            title: "Fix".into(),
            github_username: "outside-dev".into(),
            branch: "main".into(),
            merit_points: 10,
        },
    )
    .await
    .unwrap()
    .expect("main is a payout branch in payouts.toml");
    let p = |id: &str| Principal {
        token_id: format!("t-{id}"),
        org_id: DEFAULT_ORG.into(),
        member_id: id.into(),
        scopes: vec![scopes::PAYOUTS_APPROVE.into()],
        payee_keys: vec![],
    };
    approvals::approve_row(&pool, &dir, &p("ada-example"), &e.id)
        .await
        .unwrap();
    approvals::approve_row(&pool, &dir, &p("cy-example"), &e.id)
        .await
        .unwrap();
    let facts = approvals::load_facts(&pool, &dir, e.clone()).await.unwrap();
    assert_eq!(
        approvals::counted_approvals(&eff.policy, &facts.approvers),
        1,
        "only the Maintainer's approval counts"
    );
    let by: Vec<(String, Option<i64>)> = facts
        .approvers
        .iter()
        .map(|a| (a.member_id.clone(), a.rank))
        .collect();
    assert!(
        by.contains(&("ada-example".into(), Some(60)))
            && by.contains(&("cy-example".into(), Some(10))),
        "{by:?}"
    );

    // Control, no snapshot: the same ids have no rank, exactly as with EnvDirectory.
    let empty = CraDirectory::new(
        EnvDirectory::new(vec![FOUNDER.to_string()]),
        CraHandle::new(),
    )
    .with_today(today());
    let env = EnvDirectory::new(vec![FOUNDER.to_string()]);
    for id in [
        "ada-example",
        "mckale",
        "cy-example",
        "founder",
        FOUNDER,
        "nobody",
    ] {
        assert_eq!(empty.rank_of(id), env.rank_of(id), "{id}");
        assert_eq!(empty.member_for_payee(id), env.member_for_payee(id), "{id}");
        assert_eq!(empty.roles_of(id), env.roles_of(id), "{id}");
    }
}

#[tokio::test]
async fn a_member_cannot_approve_a_payout_to_their_own_foundry_login() {
    let pool = pool().await;
    let mut files = eustress();
    // Give Ada a GitLab login as well.
    files.members = files.members.replace(
        "github = \"ada-example\"",
        "github = \"ada-example\"\ngitlab = \"ada-gl\"",
    );
    let (_handle, dir) = eustress_active(&pool, files).await;
    let cfg = PaymentConfig::from_lookup(|_| None);
    let ada = Principal {
        token_id: "t".into(),
        org_id: DEFAULT_ORG.into(),
        member_id: "ada-example".into(),
        scopes: vec![scopes::PAYOUTS_APPROVE.into()],
        payee_keys: vec![],
    };
    for (pr, payee) in [(1, "Ada-Example"), (2, "gitlab:ada-gl")] {
        let e = ledger::record_merge(
            &pool,
            &cfg,
            &ledger::MergeEvent {
                pr_id: pr,
                pr_number: pr,
                merge_sha: format!("m{pr}"),
                repo: None,
                title: "x".into(),
                github_username: payee.into(),
                branch: "main".into(),
                merit_points: 0,
            },
        )
        .await
        .unwrap()
        .unwrap();
        let r = approvals::approve_row(&pool, &dir, &ada, &e.id).await;
        assert!(
            matches!(
                r,
                Err(crate::payments::error::PayoutError::PayeeCannotApprove)
            ),
            "{payee}: {r:?}"
        );
    }
    assert_eq!(
        dir.member_for_payee("ada-example").as_deref(),
        Some("ada-example")
    );
    assert_eq!(
        dir.member_for_payee("gitlab:ada-gl").as_deref(),
        Some("ada-example")
    );
    assert_eq!(
        dir.member_for_payee("gitlab:ada-example"),
        None,
        "a GitHub login is not a GitLab login"
    );

    // Control, no snapshot: the same token (no payee keys) may approve, as before.
    let none = CraDirectory::new(EnvDirectory::default(), CraHandle::new());
    let pool2 = self::pool().await;
    let e = ledger::record_merge(
        &pool2,
        &cfg,
        &ledger::MergeEvent {
            pr_id: 1,
            pr_number: 1,
            merge_sha: "m1".into(),
            repo: None,
            title: "x".into(),
            github_username: "ada-example".into(),
            branch: "main".into(),
            merit_points: 0,
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert!(approvals::approve_row(&pool2, &none, &ada, &e.id)
        .await
        .is_ok());
}

#[tokio::test]
async fn a_mirror_repository_in_org_toml_earns_nothing() {
    let pool = pool().await;
    let mut files = eustress();
    files.org.push_str("\n[[foundry]]\nprovider = \"gitlab\"\nrepo = \"example-eustress/eustress-engine\"\nvisibility = \"public\"\ncanonical = false\n");
    let (handle, _dir) = eustress_active(&pool, files).await;
    let reg = crate::routes::foundry_registry_for(&handle, &RepoRegistry::legacy());
    assert_eq!(
        reg.decide(
            FoundryProvider::GitLab,
            "gitlab.com",
            Some("example-eustress/eustress-engine")
        ),
        foundry_mod::RepoDecision::Mirror
    );
    assert_eq!(
        reg.decide(
            FoundryProvider::GitHub,
            "github.com",
            Some("example-eustress/eustress-engine")
        ),
        foundry_mod::RepoDecision::Earn
    );
    assert_eq!(
        reg.decide(FoundryProvider::GitHub, "github.com", Some("someone/else")),
        foundry_mod::RepoDecision::Unlisted
    );

    // A merge on the mirror through the real GitLab webhook path: nothing earned, no ledger row.
    use actix_web::test::TestRequest;
    let h = TestRequest::post()
        .insert_header(("X-Gitlab-Token", "s"))
        .to_http_request()
        .headers()
        .clone();
    let pay = Arc::new(PaymentConfig::from_lookup(|_| None));
    let rules = crate::routes::merit_rules_for(&handle);
    for action in ["open", "merge"] {
        let b = serde_json::to_vec(&crate::foundry::gitlab::tests::mr_hook(
            action,
            (5, "carol"),
            5,
            "example-eustress/eustress-engine",
            "https://gitlab.com/example-eustress/eustress-engine",
        ))
        .unwrap();
        let (st, j) = crate::routes::process_gitlab_webhook(
            &pool,
            Some("s"),
            reg.clone(),
            pay.clone(),
            rules.clone(),
            &h,
            &b,
        )
        .await;
        assert_eq!(
            (st.as_u16(), j["status"].as_str()),
            (200, Some("ignored")),
            "{action}"
        );
    }
    let ledger_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payout_ledger")
        .fetch_one(&pool)
        .await
        .unwrap();
    let merit: i64 =
        sqlx::query_scalar("SELECT COALESCE(SUM(total_merit_points), 0) FROM contributors")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((ledger_rows, merit), (0, 0));

    // Control, no snapshot: the env registry is used unchanged.
    let env =
        RepoRegistry::from_spec("gitlab:gitlab.com:example-eustress/eustress-engine").unwrap();
    let reg = crate::routes::foundry_registry_for(&CraHandle::new(), &env);
    assert_eq!(*reg, env);
    assert_eq!(
        *crate::routes::merit_rules_for(&CraHandle::new()),
        MeritRules::default()
    );
}

#[test]
fn merit_rules_come_from_merit_toml() {
    let snap = crate::cra::load(&eustress(), today()).unwrap();
    let r = merit_rules_from_cra(&snap.merit);
    assert_eq!(r.rubric, "msync-rubric-v1");
    assert_eq!(r.scorers, vec![ScorerSource::Agent]);
    assert_eq!((r.min_scorers, r.score_window_days), (1, 7));
    assert_eq!((r.pr_merged.base_points, r.pr_merged.max_points), (10, 60));
    assert_eq!(
        (r.review_approved.base_points, r.review_approved.max_points),
        (10, 10)
    );
    assert_eq!(
        (r.pr_opened.base_points, r.pr_opened.max_points),
        (0, 0),
        "opening a PR is not landed work"
    );
    // An event the file does not list earns nothing.
    let mut m = snap.merit.clone();
    m.events.retain(|e| e.key != EventKind::ReviewApproved);
    assert_eq!(merit_rules_from_cra(&m).review_approved.points(None), 0);
}

async fn token(pool: &SqlitePool, member: &str, scopes_: &[&str]) -> String {
    crate::auth::create_token(
        pool,
        &EnvDirectory::default(),
        crate::auth::TokenCreator::Bootstrap,
        crate::auth::NewToken {
            label: "t".into(),
            member_id: member.into(),
            scopes: scopes_.iter().map(|s| s.to_string()).collect(),
            payee_keys: vec![],
            expires_at: None,
        },
        Utc::now(),
    )
    .await
    .unwrap()
    .token
}

#[actix_web::test]
async fn put_payout_policy_is_refused_with_409_while_a_snapshot_is_active() {
    use actix_web::{test as atest, web, App};
    let pool = pool().await;
    let tok = token(
        &pool,
        "mckale",
        &[scopes::PAYOUTS_POLICY, scopes::PAYOUTS_READ],
    )
    .await;
    let handle = CraHandle::new();
    let dir: Arc<dyn Directory> = Arc::new(CraDirectory::new(
        EnvDirectory::new(vec![FOUNDER.into()]),
        handle.clone(),
    ));
    let cra = web::Data::new(CraContext::disabled(
        pool.clone(),
        handle.clone(),
        dir.clone(),
    ));
    let api = web::Data::new(crate::payments::api::ApiContext {
        pool: pool.clone(),
        payments: Arc::new(PaymentConfig::from_lookup(|_| None)),
        stripe: Arc::new(crate::payments::StripeClient::new(None, false)),
        directory: dir,
    });
    let app = atest::init_service(
        App::new()
            .app_data(cra.clone())
            .app_data(api)
            .configure(crate::cra::api::configure)
            .configure(crate::payments::api::configure),
    )
    .await;
    let body = json!({"schema": 1, "enabled": true, "currency": "usd", "schedule": {"cadence": "manual"}, "limits": {},
        "approval": {"approvals_required": 1, "min_approver_rank": 0}, "rate": [{"branch": "main", "base_cents": 100, "merit_multiplier": 0.0}]});
    let put = |b: &serde_json::Value| {
        atest::TestRequest::put()
            .uri("/api/v1/payouts/policy")
            .insert_header(("Authorization", format!("Bearer {tok}")))
            .set_json(b.clone())
            .to_request()
    };

    // No snapshot: exactly as before (stored as version 1).
    let resp = atest::call_service(&app, put(&body)).await;
    assert_eq!(resp.status().as_u16(), 200);

    // Snapshot active: 409 with the CRA message, and nothing stored.
    let (h2, _) = eustress_active(&pool, eustress()).await;
    handle.replace(h2.active());
    let resp = atest::call_service(&app, put(&body)).await;
    assert_eq!(resp.status().as_u16(), 409);
    let j: serde_json::Value = atest::read_body_json(resp).await;
    assert!(
        j["message"]
            .as_str()
            .unwrap()
            .contains("authored in the CRA"),
        "{j}"
    );
    let versions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payout_policies")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(versions, 1);
    // Unauthenticated callers still get 401, not a hint.
    let resp = atest::call_service(
        &app,
        atest::TestRequest::put()
            .uri("/api/v1/payouts/policy")
            .set_json(body.clone())
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 401);
    // The library refuses too, whatever the route.
    let p = Principal {
        token_id: "t".into(),
        org_id: DEFAULT_ORG.into(),
        member_id: "mckale".into(),
        scopes: vec![scopes::PAYOUTS_POLICY.into()],
        payee_keys: vec![],
    };
    let pol: pay_policy::PayoutPolicy = serde_json::from_value(body).unwrap();
    assert!(matches!(
        pay_policy::put_policy(&pool, &p, &pol, None).await,
        Err(pay_policy::PolicyError::Invalid(_))
    ));
    // GET shows the CRA policy, not the stored one.
    let resp = atest::call_service(
        &app,
        atest::TestRequest::get()
            .uri("/api/v1/payouts/policy")
            .insert_header(("Authorization", format!("Bearer {tok}")))
            .to_request(),
    )
    .await;
    let j: serde_json::Value = atest::read_body_json(resp).await;
    assert_eq!(j["policy"]["approval"]["min_approver_rank"], 60);
}

#[tokio::test]
async fn activation_links_member_identities_and_reports_conflicts() {
    let pool = pool().await;
    // carol earned under GitLab before the CRA existed; an unrelated contributor holds `ada-gl`.
    crate::foundry::identity::resolve_contributor(
        &pool,
        FoundryProvider::GitLab,
        "gitlab.com",
        "carol-gl",
    )
    .await
    .unwrap();
    crate::foundry::identity::resolve_contributor(
        &pool,
        FoundryProvider::GitHub,
        "github.com",
        "ada-example",
    )
    .await
    .unwrap();
    crate::foundry::identity::resolve_contributor(
        &pool,
        FoundryProvider::GitLab,
        "gitlab.com",
        "ada-gl",
    )
    .await
    .unwrap();
    let mut files = eustress();
    files.members = files
        .members
        .replace(
            "github = \"ada-example\"",
            "github = \"ada-example\"\ngitlab = \"ada-gl\"",
        )
        .replace(
            "github = \"cy-example\"",
            "github = \"cy-example\"\ngitlab = \"carol-gl\"",
        );
    let src = FakeSource::new();
    src.commit(&sha(1), Ok(files), Ok(direct("example-founder")));
    let LoadOutcome::Activated { identity_links, .. } = load_commit(
        &pool,
        &src,
        &repo(),
        &sha(1),
        None,
        &[FOUNDER.to_string()],
        today(),
    )
    .await
    .unwrap() else {
        panic!()
    };
    let find = |m: &str, id: &str| {
        identity_links
            .iter()
            .find(|r| r.member_id == m && r.identity == id)
            .map(|r| r.result.clone())
    };
    assert_eq!(
        find("ada-example", "gitlab:gitlab.com:ada-gl").as_deref(),
        Some("conflict"),
        "{identity_links:?}"
    );
    assert_eq!(
        find("ada-example", "github:github.com:ada-example").as_deref(),
        Some("linked")
    );
    // cy had no GitHub contributor; the GitLab one is primary and the GitHub login joins it.
    assert_eq!(
        find("cy-example", "github:github.com:cy-example").as_deref(),
        Some("linked")
    );
    let a = crate::foundry::identity::find_contributor(
        &pool,
        FoundryProvider::GitHub,
        "github.com",
        "cy-example",
    )
    .await
    .unwrap()
    .unwrap();
    let b = crate::foundry::identity::find_contributor(
        &pool,
        FoundryProvider::GitLab,
        "gitlab.com",
        "carol-gl",
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(a.id, b.id, "one person, one contributor: XP adds up");
    let x = crate::foundry::identity::find_contributor(
        &pool,
        FoundryProvider::GitHub,
        "github.com",
        "ada-example",
    )
    .await
    .unwrap()
    .unwrap();
    let y = crate::foundry::identity::find_contributor(
        &pool,
        FoundryProvider::GitLab,
        "gitlab.com",
        "ada-gl",
    )
    .await
    .unwrap()
    .unwrap();
    assert_ne!(x.id, y.id, "a conflict is reported, never merged");
}

#[tokio::test]
async fn hierarchy_and_standing_lookups() {
    let pool = pool().await;
    let (handle, dir) = eustress_active(&pool, eustress()).await;
    let h = crate::cra::api::hierarchy_get(&handle).unwrap();
    let hv = serde_json::to_value(&h).unwrap();
    assert_eq!(
        (hv["org_slug"].as_str(), hv["mode"].as_str()),
        (Some("eustress-engine"), Some("source_available"))
    );
    let ranks: Vec<u32> = h.roles.iter().map(|r| r.rank).collect();
    assert!(ranks.windows(2).all(|w| w[0] >= w[1]), "{ranks:?}");
    assert_eq!(h.roles[0].key, "lead-maintainer");

    crate::foundry::identity::resolve_contributor(
        &pool,
        FoundryProvider::GitHub,
        "github.com",
        "cy-example",
    )
    .await
    .unwrap();
    sqlx::query("UPDATE contributors SET total_merit_points = 160")
        .execute(&pool)
        .await
        .unwrap();
    let v = crate::cra::api::standing_get(&pool, &handle, &dir, "cy-example", today())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(v.vested_merit, 160);
    assert_eq!(
        v.standing.tier.as_ref().map(|t| t.key.as_str()),
        Some("active-contributor")
    );
    assert_eq!(v.standing.rank, 20);
    let f = crate::cra::api::standing_get(&pool, &handle, &dir, "founder", today())
        .await
        .unwrap()
        .unwrap();
    assert!(f.owner);
    assert_eq!(f.effective_rank, Some(1000));
    assert!(
        crate::cra::api::standing_get(&pool, &handle, &dir, "nobody", today())
            .await
            .unwrap()
            .is_none()
    );
    assert!(crate::cra::api::hierarchy_get(&CraHandle::new()).is_none());
    let list = crate::cra::api::snapshots_list(&pool, 10).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].status, "active");
}

#[actix_web::test]
async fn read_endpoints_need_hierarchy_read_and_reload_needs_an_owner() {
    use actix_web::{test as atest, web, App};
    let pool = pool().await;
    let src = Arc::new(FakeSource::new());
    src.commit(&sha(1), Ok(eustress()), Ok(direct("example-founder")));
    *src.head.lock().unwrap() = Some(sha(1));
    let handle = CraHandle::new();
    let dir: Arc<dyn Directory> = Arc::new(
        CraDirectory::new(
            EnvDirectory::new(vec![FOUNDER.into(), "root".into()]),
            handle.clone(),
        )
        .with_today(today()),
    );
    let ctx = web::Data::new(CraContext::new(
        pool.clone(),
        handle.clone(),
        dir,
        vec![FOUNDER.into()],
        Some(CraSettings {
            repo: repo(),
            branch: "main".into(),
        }),
        Some(src.clone() as Arc<dyn CraSource>),
    ));
    let app = atest::init_service(
        App::new()
            .app_data(ctx.clone())
            .configure(crate::cra::api::configure),
    )
    .await;
    let reader = token(&pool, "ada-example", &[scopes::HIERARCHY_READ]).await;
    let owner = token(&pool, "root", &[scopes::HIERARCHY_READ]).await;
    let other = token(&pool, "root", &[scopes::PAYOUTS_READ]).await;
    let get = |uri: &str, t: &str| {
        atest::TestRequest::get()
            .uri(uri)
            .insert_header(("Authorization", format!("Bearer {t}")))
            .to_request()
    };
    let post = |uri: &str, t: &str| {
        atest::TestRequest::post()
            .uri(uri)
            .insert_header(("Authorization", format!("Bearer {t}")))
            .to_request()
    };

    assert_eq!(
        atest::call_service(&app, get("/api/v1/hierarchy", &reader))
            .await
            .status()
            .as_u16(),
        404,
        "nothing active yet"
    );
    assert_eq!(
        atest::call_service(&app, get("/api/v1/hierarchy", &other))
            .await
            .status()
            .as_u16(),
        403
    );
    assert_eq!(
        atest::call_service(
            &app,
            atest::TestRequest::get()
                .uri("/api/v1/hierarchy")
                .to_request()
        )
        .await
        .status()
        .as_u16(),
        401
    );
    assert_eq!(
        atest::call_service(&app, post("/api/v1/cra/reload", &reader))
            .await
            .status()
            .as_u16(),
        403,
        "a non-owner cannot reload"
    );
    assert!(src.fetched().is_empty());

    // The owner reloads the branch head (the fake source attributes it to the founder).
    let resp = atest::call_service(&app, post("/api/v1/cra/reload", &owner)).await;
    assert_eq!(resp.status().as_u16(), 200);
    let j: serde_json::Value = atest::read_body_json(resp).await;
    assert_eq!(j["result"]["outcome"], "activated", "{j}");
    assert!(handle.active().is_some());

    let resp = atest::call_service(&app, get("/api/v1/hierarchy", &reader)).await;
    let j: serde_json::Value = atest::read_body_json(resp).await;
    assert_eq!(j["hierarchy"]["mode"], "source_available");
    assert_eq!(j["hierarchy"]["roles"][0]["key"], "lead-maintainer");
    let resp =
        atest::call_service(&app, get("/api/v1/hierarchy/members/ada-example", &reader)).await;
    let j: serde_json::Value = atest::read_body_json(resp).await;
    assert_eq!(j["member"]["standing"]["rank"], 60, "{j}");
    let resp = atest::call_service(&app, get("/api/v1/cra/snapshots", &reader)).await;
    let j: serde_json::Value = atest::read_body_json(resp).await;
    assert_eq!(j["snapshots"][0]["status"], "active");
    // Reloading the same head again does nothing.
    let resp = atest::call_service(&app, post("/api/v1/cra/reload", &owner)).await;
    let j: serde_json::Value = atest::read_body_json(resp).await;
    assert_eq!(j["result"]["outcome"], "already_processed");
}

// ------------------------------------------------------------------ story

#[tokio::test]
async fn eustress_engine_story() {
    let pool = pool().await;
    let src = FakeSource::new();
    let owners_cfg = vec![FOUNDER.to_string()];
    let run = |sha_: String| {
        let (pool, src, owners_cfg) = (&pool, &src, &owners_cfg);
        async move {
            load_commit(pool, src, &repo(), &sha_, None, owners_cfg, today())
                .await
                .unwrap()
        }
    };

    // 1. A Maintainer cannot bootstrap; the owner from engine configuration does.
    src.commit(&sha(1), Ok(eustress()), Ok(direct("ada-example")));
    assert_eq!(
        rejected_codes(&run(sha(1)).await),
        vec!["NotBootstrapOwner"]
    );
    src.commit(&sha(2), Ok(eustress()), Ok(direct("example-founder")));
    let LoadOutcome::Activated {
        snapshot_id: boot, ..
    } = run(sha(2)).await
    else {
        panic!("bootstrap")
    };

    // 2. Ada (Maintainer, 60) grants Reviewer (40) to Cy through an approved pull request.
    let mut with_reviewer = eustress();
    with_reviewer
        .members
        .push_str("\n[[member.grant]]\nrole = \"reviewer\"\ngranted_by = \"ada-example\"\n");
    src.commit(
        &sha(3),
        Ok(with_reviewer.clone()),
        Ok(via_pr("ada-example", "ada-example", &["Simbuilder"])),
    );
    let LoadOutcome::Activated {
        snapshot_id: second,
        plan_markdown,
        ..
    } = run(sha(3)).await
    else {
        panic!("reviewer grant")
    };
    assert!(plan_markdown.contains("reviewer"), "{plan_markdown}");
    let handle = CraHandle::new();
    handle.reload_from_store(&pool, ORG_ID).await.unwrap();
    let dir = CraDirectory::new(EnvDirectory::new(owners_cfg.clone()), handle.clone())
        .with_today(today());
    assert_eq!(dir.rank_of("cy-example"), Some(40));

    // 3. Ada tries to make Cy a Maintainer: her own rank. Approved by the Lead, still refused.
    let mut with_maintainer = with_reviewer.clone();
    // Inside the 180-day source_available term, so the only fault is the rank.
    with_maintainer.members.push_str("\n[[member.grant]]\nrole = \"maintainer\"\ngranted_by = \"ada-example\"\nexpires = \"2027-05-01\"\n");
    src.commit(
        &sha(4),
        Ok(with_maintainer),
        Ok(via_pr("ada-example", "ada-example", &["Simbuilder"])),
    );
    let o = run(sha(4)).await;
    assert_eq!(rejected_codes(&o), vec!["InsufficientRank"], "{o:?}");
    assert_eq!(
        active_id(&pool).await,
        Some(second),
        "the active snapshot is unchanged"
    );
    let LoadOutcome::Rejected { snapshot_id, .. } = o else {
        unreachable!()
    };
    let row = store::get(&pool, snapshot_id).await.unwrap().unwrap();
    assert_eq!(row.violations.len(), 1);
    assert!(
        row.violations[0]
            .item
            .contains("grant `maintainer` to `cy-example`"),
        "{:?}",
        row.violations
    );
    assert!(
        row.violations[0].message.contains("rank 60"),
        "{:?}",
        row.violations
    );
    assert_eq!(row.approvers, vec!["github:Simbuilder"]);
    handle.reload_from_store(&pool, ORG_ID).await.unwrap();
    assert_eq!(
        dir.rank_of("cy-example"),
        Some(40),
        "Cy is still a Reviewer"
    );
    let statuses: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, status FROM cra_snapshots ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        statuses.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>(),
        vec!["rejected", "superseded", "active", "rejected"]
    );
    assert_eq!(statuses[1].0, boot);
}
