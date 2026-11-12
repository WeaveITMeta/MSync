//! Multi-repository XP (contract 5.1): one summed score per person, with a
//! per-repository drilldown that always adds up to the total.
//!
//! The total is `contributors.total_merit_points` (the existing leaderboard value).
//! Each `merit_activities` row carries `(provider, host, repo)`; rows without one
//! (legacy rows, manual awards) and any points not backed by an activity row fall in
//! `unattributed`, so the drilldown sums exactly to the total.
//!
//! With `include_private_names = false`, every repository that is not public in the
//! registry collapses into one `private repositories` line and its name never appears.

use crate::forge::{RepoRef, RepoRegistry};
use serde::Serialize;
use sqlx::{Row, SqlitePool};
use std::collections::BTreeMap;
use uuid::Uuid;

pub const PRIVATE_LABEL: &str = "private repositories";
pub const UNATTRIBUTED_LABEL: &str = "unattributed";

/// contributors.id may be stored as a 16-byte BLOB (sqlx Uuid) or as TEXT; normalise to
/// lowercase hyphenated text so joins and comparisons work for both.
fn norm_id(col: &str) -> String {
    format!(
        "(CASE WHEN typeof({c}) = 'blob' THEN lower(substr(hex({c}),1,8)||'-'||substr(hex({c}),9,4)||'-'||substr(hex({c}),13,4)||'-'||substr(hex({c}),17,4)||'-'||substr(hex({c}),21)) ELSE lower({c}) END)",
        c = col
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BucketKind {
    Repo,
    PrivateRepositories,
    Unattributed,
}

/// One drilldown line. `provider`/`host`/`repo` are None for the combined private line
/// and for `unattributed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoPoints {
    pub kind: BucketKind,
    pub label: String,
    pub provider: Option<String>,
    pub host: Option<String>,
    pub repo: Option<String>,
    pub private: bool,
    pub points: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KindPoints {
    pub kind: String,
    pub points: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LeaderboardRow {
    pub rank: usize,
    pub contributor_id: String,
    pub github_username: String,
    pub current_role: String,
    pub contributions_count: i64,
    pub total_points: i64,
    pub by_repo: Vec<RepoPoints>,
}

#[derive(Debug, Clone, Default)]
pub struct LeaderboardQuery {
    /// Rank by XP earned in this repository alone.
    pub repo: Option<RepoRef>,
    pub limit: usize,
    pub include_private_names: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemberMerit {
    pub contributor_id: String,
    pub github_username: String,
    pub total_points: i64,
    pub by_repo: Vec<RepoPoints>,
    pub by_kind: Vec<KindPoints>,
}

struct Person {
    id: String,
    github_username: String,
    current_role: String,
    contributions_count: i64,
    total: i64,
}

/// (contributor id, repo or None, activity kind, points)
type ActRow = (String, Option<RepoRef>, String, i64);

fn kind_name(raw: &str) -> String {
    match raw {
        "0" => "pull_request_opened",
        "1" => "pull_request_merged",
        "2" => "code_review",
        "3" => "issue_resolved",
        "4" => "documentation_update",
        "5" => "community_help",
        "6" => "bug_fix",
        "7" => "feature_implementation",
        "8" => "security_fix",
        other => return other.to_string(),
    }
    .to_string()
}

async fn load_people(pool: &SqlitePool, only: Option<&str>) -> Result<Vec<Person>, sqlx::Error> {
    let sql = format!(
        "SELECT {id} AS cid, github_username, current_role, contributions_count, total_merit_points FROM contributors{w}",
        id = norm_id("id"),
        w = if only.is_some() { format!(" WHERE {} = ?", norm_id("id")) } else { String::new() }
    );
    let mut q = sqlx::query(&sql);
    if let Some(id) = only {
        q = q.bind(id.to_ascii_lowercase());
    }
    q.fetch_all(pool)
        .await?
        .into_iter()
        .map(|r| {
            Ok(Person {
                id: r.try_get("cid")?,
                github_username: r.try_get("github_username")?,
                current_role: r.try_get("current_role")?,
                contributions_count: r.try_get("contributions_count")?,
                total: r.try_get("total_merit_points")?,
            })
        })
        .collect()
}

async fn load_activity(pool: &SqlitePool, only: Option<&str>) -> Result<Vec<ActRow>, sqlx::Error> {
    let sql = format!(
        "SELECT {id} AS cid, provider, host, repo, CAST(activity_type AS TEXT) AS kind, SUM(merit_change) AS pts
         FROM merit_activities{w} GROUP BY 1, 2, 3, 4, 5",
        id = norm_id("contributor_id"),
        w = if only.is_some() { format!(" WHERE {} = ?", norm_id("contributor_id")) } else { String::new() }
    );
    let mut q = sqlx::query(&sql);
    if let Some(id) = only {
        q = q.bind(id.to_ascii_lowercase());
    }
    q.fetch_all(pool)
        .await?
        .into_iter()
        .map(|r| {
            let provider: Option<String> = r.try_get("provider")?;
            let host: Option<String> = r.try_get("host")?;
            let repo: Option<String> = r.try_get("repo")?;
            let repo_ref = match (provider, host, repo) {
                (Some(p), Some(h), Some(rp)) => Some(RepoRef { provider: p, host: h, repo: rp }),
                _ => None,
            };
            let kind: Option<String> = r.try_get("kind")?;
            Ok((r.try_get("cid")?, repo_ref, kind_name(&kind.unwrap_or_default()), r.try_get("pts")?))
        })
        .collect()
}

/// Drilldown for one person. Always sums to `total`.
fn drilldown(total: i64, acts: &[&ActRow], registry: &RepoRegistry, include_private_names: bool) -> Vec<RepoPoints> {
    let mut named: BTreeMap<RepoRef, i64> = BTreeMap::new();
    let mut private_sum = 0i64;
    let mut attributed = 0i64;
    for (_, r, _, pts) in acts {
        let Some(r) = r else { continue };
        attributed += pts;
        if include_private_names || registry.is_public(r) {
            *named.entry(r.clone()).or_default() += pts;
        } else {
            private_sum += pts;
        }
    }
    let unattributed = total - attributed;
    let mut out: Vec<RepoPoints> = named
        .into_iter()
        .filter(|(_, p)| *p != 0)
        .map(|(r, points)| RepoPoints {
            kind: BucketKind::Repo,
            label: format!("{}/{}", r.host, r.repo),
            private: !registry.is_public(&r),
            provider: Some(r.provider),
            host: Some(r.host),
            repo: Some(r.repo),
            points,
        })
        .collect();
    if private_sum != 0 {
        out.push(RepoPoints { kind: BucketKind::PrivateRepositories, label: PRIVATE_LABEL.into(), provider: None, host: None, repo: None, private: true, points: private_sum });
    }
    if unattributed != 0 {
        out.push(RepoPoints { kind: BucketKind::Unattributed, label: UNATTRIBUTED_LABEL.into(), provider: None, host: None, repo: None, private: false, points: unattributed });
    }
    out.sort_by(|a, b| b.points.cmp(&a.points).then_with(|| a.label.cmp(&b.label)));
    out
}

/// Leaderboard. Without `repo`: total XP per person, drilldown by repository. With
/// `repo`: ranks by that repository's XP alone; people with none are left out. A
/// private repository queried without `include_private_names` returns nothing.
pub async fn leaderboard(pool: &SqlitePool, registry: &RepoRegistry, q: LeaderboardQuery) -> Result<Vec<LeaderboardRow>, sqlx::Error> {
    let people = load_people(pool, None).await?;
    let acts = load_activity(pool, None).await?;
    let mut by_person: BTreeMap<&str, Vec<&ActRow>> = BTreeMap::new();
    for a in &acts {
        by_person.entry(a.0.as_str()).or_default().push(a);
    }

    let target = q.repo.as_ref().map(|r| RepoRef { provider: r.provider.to_ascii_lowercase(), host: r.host.to_ascii_lowercase(), repo: r.repo.trim_matches('/').to_ascii_lowercase() });
    if let Some(t) = &target {
        if !q.include_private_names && !registry.is_public(t) {
            return Ok(Vec::new());
        }
    }

    let mut rows: Vec<LeaderboardRow> = Vec::new();
    for p in &people {
        let mine = by_person.get(p.id.as_str()).cloned().unwrap_or_default();
        let (total, by_repo) = match &target {
            None => (p.total, drilldown(p.total, &mine, registry, q.include_private_names)),
            Some(t) => {
                let only: Vec<&ActRow> = mine.into_iter().filter(|a| a.1.as_ref() == Some(t)).collect();
                let sum: i64 = only.iter().map(|a| a.3).sum();
                if sum == 0 {
                    continue;
                }
                (sum, drilldown(sum, &only, registry, q.include_private_names))
            }
        };
        rows.push(LeaderboardRow {
            rank: 0,
            contributor_id: p.id.clone(),
            github_username: p.github_username.clone(),
            current_role: p.current_role.clone(),
            contributions_count: p.contributions_count,
            total_points: total,
            by_repo,
        });
    }
    rows.sort_by(|a, b| b.total_points.cmp(&a.total_points).then_with(|| a.github_username.cmp(&b.github_username)));
    if q.limit > 0 {
        rows.truncate(q.limit);
    }
    for (i, r) in rows.iter_mut().enumerate() {
        r.rank = i + 1;
    }
    Ok(rows)
}

/// One person's XP by repository and by event kind. Both lists sum to the total.
pub async fn member_merit(pool: &SqlitePool, registry: &RepoRegistry, contributor_id: Uuid, include_private_names: bool) -> Result<Option<MemberMerit>, sqlx::Error> {
    let id = contributor_id.to_string();
    let Some(p) = load_people(pool, Some(&id)).await?.into_iter().next() else { return Ok(None) };
    let acts = load_activity(pool, Some(&id)).await?;
    let refs: Vec<&ActRow> = acts.iter().collect();
    let by_repo = drilldown(p.total, &refs, registry, include_private_names);

    let mut kinds: BTreeMap<String, i64> = BTreeMap::new();
    let mut sum = 0;
    for (_, _, k, pts) in &acts {
        *kinds.entry(k.clone()).or_default() += pts;
        sum += pts;
    }
    if p.total - sum != 0 {
        *kinds.entry("other".into()).or_default() += p.total - sum;
    }
    let mut by_kind: Vec<KindPoints> = kinds.into_iter().filter(|(_, v)| *v != 0).map(|(kind, points)| KindPoints { kind, points }).collect();
    by_kind.sort_by(|a, b| b.points.cmp(&a.points).then_with(|| a.kind.cmp(&b.kind)));

    Ok(Some(MemberMerit { contributor_id: p.id, github_username: p.github_username, total_points: p.total, by_repo, by_kind }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::gitlab::tests::mr_hook;
    use crate::forge::identity::{find_contributor, link_forge_identity, LinkError};
    use crate::forge::{github, gitlab, ForgeProcessor, ForgeProvider};
    use crate::github::webhook::GitHubWebhookPayload;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::sync::Arc;

    const SPEC: &str = "github:github.com:org/alpha, github:github.com:org/beta, gitlab:gitlab.com:grp/proj, github:github.com:org/secret-sauce:private";

    async fn pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn reg() -> RepoRegistry {
        RepoRegistry::from_spec(SPEC).unwrap()
    }

    fn proc_(pool: &SqlitePool) -> ForgeProcessor {
        ForgeProcessor::new(pool.clone()).with_registry(Arc::new(reg()))
    }

    /// Open + merge a GitHub PR by `login` in `repo` (5 + 10 points).
    async fn gh_pr(p: &ForgeProcessor, repo: &str, id: i64, login: &str) {
        for (action, merged) in [("opened", false), ("closed", true)] {
            let payload: GitHubWebhookPayload = serde_json::from_value(serde_json::json!({
                "action": action, "repository": {"full_name": repo},
                "pull_request": {"id": id, "number": 1, "title": "T", "body": null, "state": "x", "merged": merged,
                    "user": {"login": login}, "merge_commit_sha": "abc", "base": {"ref": "main"}}
            }))
            .unwrap();
            let ev = github::normalize("pull_request", &payload, None).unwrap();
            p.process(&ev).await.unwrap();
        }
    }

    /// Open + merge a GitLab MR by `login` in grp/proj (5 + 10 points).
    async fn gl_mr(p: &ForgeProcessor, login: &str) {
        for action in ["open", "merge"] {
            let payload: gitlab::GitLabWebhookPayload =
                serde_json::from_value(mr_hook(action, (77, login), 77, "grp/proj", "https://gitlab.com/grp/proj")).unwrap();
            let ev = gitlab::normalize(&payload, None).unwrap().unwrap();
            p.process(&ev).await.unwrap();
        }
    }

    fn sum(lines: &[RepoPoints]) -> i64 {
        lines.iter().map(|l| l.points).sum()
    }

    #[tokio::test]
    async fn drilldown_sums_to_total_across_two_github_repos_and_gitlab_under_linked_identities() {
        let pool = pool().await;
        let p = proc_(&pool);
        gh_pr(&p, "org/alpha", 1, "alice").await;
        gh_pr(&p, "org/beta", 2, "alice").await;
        let alice = find_contributor(&pool, ForgeProvider::GitHub, "github.com", "alice").await.unwrap().unwrap();
        link_forge_identity(&pool, alice.id, ForgeProvider::GitLab, "gitlab.com", "alice-gl").await.unwrap();
        gl_mr(&p, "alice-gl").await;

        let m = member_merit(&pool, &reg(), alice.id, true).await.unwrap().unwrap();
        assert_eq!(m.total_points, 45);
        assert_eq!(sum(&m.by_repo), m.total_points);
        let labels: Vec<&str> = m.by_repo.iter().map(|l| l.label.as_str()).collect();
        assert_eq!(labels, vec!["github.com/org/alpha", "github.com/org/beta", "gitlab.com/grp/proj"]);
        assert!(m.by_repo.iter().all(|l| l.points == 15));
        assert_eq!(m.by_kind.iter().map(|k| k.points).sum::<i64>(), m.total_points);
        assert_eq!(m.by_kind, vec![
            KindPoints { kind: "pull_request_merged".into(), points: 30 },
            KindPoints { kind: "pull_request_opened".into(), points: 15 },
        ]);

        let rows = leaderboard(&pool, &reg(), LeaderboardQuery { repo: None, limit: 10, include_private_names: false }).await.unwrap();
        assert_eq!(rows.len(), 1, "two identities, one person");
        assert_eq!(rows[0].total_points, 45);
        assert_eq!(sum(&rows[0].by_repo), 45);
    }

    #[tokio::test]
    async fn private_repo_never_named_publicly_but_counts() {
        let pool = pool().await;
        let p = proc_(&pool);
        gh_pr(&p, "org/alpha", 1, "alice").await;
        gh_pr(&p, "org/secret-sauce", 2, "alice").await;

        let rows = leaderboard(&pool, &reg(), LeaderboardQuery { repo: None, limit: 25, include_private_names: false }).await.unwrap();
        let json = serde_json::to_string(&crate::routes::public_leaderboard_json(&rows)).unwrap();
        assert!(!json.contains("secret-sauce"), "private name leaked: {json}");
        assert!(json.contains(PRIVATE_LABEL));
        assert_eq!(rows[0].total_points, 30);
        assert_eq!(sum(&rows[0].by_repo), 30);
        let private = rows[0].by_repo.iter().find(|l| l.kind == BucketKind::PrivateRepositories).unwrap();
        assert_eq!(private.points, 15);
        let json_rows = serde_json::to_string(&rows).unwrap();
        assert!(!json_rows.contains("secret-sauce"));

        // Asking for the private repository by name on a public query reveals nothing.
        let q = LeaderboardQuery { repo: Some(RepoRef::new(ForgeProvider::GitHub, "github.com", "org/secret-sauce")), limit: 25, include_private_names: false };
        assert!(leaderboard(&pool, &reg(), q).await.unwrap().is_empty());

        // An authorized caller sees the name.
        let rows = leaderboard(&pool, &reg(), LeaderboardQuery { repo: None, limit: 25, include_private_names: true }).await.unwrap();
        assert!(serde_json::to_string(&rows).unwrap().contains("secret-sauce"));
    }

    #[tokio::test]
    async fn repo_query_ranks_by_that_repo_alone() {
        let pool = pool().await;
        let p = proc_(&pool);
        // bob leads overall (3 PRs in beta), alice leads alpha (1 PR vs none)
        gh_pr(&p, "org/beta", 10, "bob").await;
        gh_pr(&p, "org/beta", 11, "bob").await;
        gh_pr(&p, "org/beta", 12, "bob").await;
        gh_pr(&p, "org/alpha", 20, "alice").await;
        gh_pr(&p, "org/beta", 21, "alice").await;

        let all = leaderboard(&pool, &reg(), LeaderboardQuery { repo: None, limit: 25, include_private_names: false }).await.unwrap();
        assert_eq!(all.iter().map(|r| (r.github_username.as_str(), r.total_points)).collect::<Vec<_>>(), vec![("bob", 45), ("alice", 30)]);

        let q = LeaderboardQuery { repo: Some(RepoRef::new(ForgeProvider::GitHub, "github.com", "Org/Alpha")), limit: 25, include_private_names: false };
        let alpha = leaderboard(&pool, &reg(), q).await.unwrap();
        assert_eq!(alpha.iter().map(|r| (r.rank, r.github_username.as_str(), r.total_points)).collect::<Vec<_>>(), vec![(1, "alice", 15)]);
        assert_eq!(sum(&alpha[0].by_repo), 15);

        let q = LeaderboardQuery { repo: Some(RepoRef::new(ForgeProvider::GitHub, "github.com", "org/beta")), limit: 1, include_private_names: false };
        let beta = leaderboard(&pool, &reg(), q).await.unwrap();
        assert_eq!(beta.iter().map(|r| (r.github_username.as_str(), r.total_points)).collect::<Vec<_>>(), vec![("bob", 45)]);
    }

    #[tokio::test]
    async fn linking_an_identity_owned_by_another_contributor_is_refused() {
        let pool = pool().await;
        let p = proc_(&pool);
        gh_pr(&p, "org/alpha", 1, "alice").await;
        gl_mr(&p, "bob-gl").await;
        let alice = find_contributor(&pool, ForgeProvider::GitHub, "github.com", "alice").await.unwrap().unwrap();
        let bob = find_contributor(&pool, ForgeProvider::GitLab, "gitlab.com", "bob-gl").await.unwrap().unwrap();

        // Owned through forge_identities
        let e = link_forge_identity(&pool, alice.id, ForgeProvider::GitLab, "gitlab.com", "BOB-GL").await;
        assert!(matches!(e, Err(LinkError::Conflict(_))), "{e:?}");
        // Owned through the legacy github_username key only (no identity row)
        sqlx::query("DELETE FROM forge_identities WHERE login = 'alice'").execute(&pool).await.unwrap();
        let e = link_forge_identity(&pool, bob.id, ForgeProvider::GitHub, "github.com", "alice").await;
        assert!(matches!(e, Err(LinkError::Conflict(_))), "{e:?}");
        // Unknown contributor
        let e = link_forge_identity(&pool, Uuid::new_v4(), ForgeProvider::GitHub, "github.com", "zed").await;
        assert!(matches!(e, Err(LinkError::UnknownContributor)), "{e:?}");
        // Same contributor again: idempotent
        link_forge_identity(&pool, bob.id, ForgeProvider::GitLab, "gitlab.com", "bob-gl").await.unwrap();
        // Nothing moved
        let still = find_contributor(&pool, ForgeProvider::GitLab, "gitlab.com", "bob-gl").await.unwrap().unwrap();
        assert_eq!(still.id, bob.id);
    }

    #[tokio::test]
    async fn legacy_rows_land_in_unattributed() {
        let pool = pool().await;
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO contributors (id, github_username, total_merit_points, current_role, contributions_count, created_at, updated_at)
                     VALUES (?, 'old-timer', 40, 'Newcomer', 2, datetime('now'), datetime('now'))")
            .bind(id).execute(&pool).await.unwrap();
        // Legacy activity rows: no provider/host/repo
        for pts in [25, 10] {
            sqlx::query("INSERT INTO merit_activities (id, contributor_id, activity_type, merit_change, description, created_at)
                         VALUES (?, ?, 1, ?, 'legacy', datetime('now'))")
                .bind(Uuid::new_v4()).bind(id).bind(pts).execute(&pool).await.unwrap();
        }
        // and 5 points with no activity row at all
        let m = member_merit(&pool, &reg(), id, false).await.unwrap().unwrap();
        assert_eq!(m.total_points, 40);
        assert_eq!(m.by_repo.len(), 1);
        assert_eq!(m.by_repo[0].kind, BucketKind::Unattributed);
        assert_eq!(m.by_repo[0].label, UNATTRIBUTED_LABEL);
        assert_eq!(m.by_repo[0].points, 40);
        assert_eq!(m.by_kind.iter().map(|k| k.points).sum::<i64>(), 40);
        let rows = leaderboard(&pool, &reg(), LeaderboardQuery { repo: None, limit: 25, include_private_names: false }).await.unwrap();
        assert_eq!(rows[0].by_repo, m.by_repo);
    }
}
