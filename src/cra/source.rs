//! Where a CRA load gets its facts: the five `.dsync/` files at an exact commit
//! and the [`ChangeContext`] of that commit (contract section 4 steps 1 and 4).
//!
//! Rules every implementation follows:
//!
//! - The files come from the forge API at the commit SHA, never from a webhook body.
//! - Only a merged pull or merge request whose merge commit IS this SHA makes the
//!   change `via_pull_request`. Anything else is a direct push.
//! - An approval counts only when the forge shows it is current: not dismissed or
//!   revoked, and given on (GitHub) or after (GitLab) the request's last commit. If
//!   the API cannot tell, the approval does not count.
//! - Any API error, missing field or ambiguous answer is an `Err`. The loader turns
//!   that into a rejected snapshot; no check is ever skipped.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use reqwest::header::{ACCEPT, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{ChangeContext, CraFileKind, CraFiles, ForgeIdentity};
use crate::forge::ForgeProvider;

/// Largest `.dsync/` file the loader accepts. Anything larger is refused.
pub const MAX_FILE_BYTES: usize = 512 * 1024;
const USER_AGENT_VALUE: &str = "dsync-cra-loader";
const PER_PAGE: usize = 100;
const MAX_PAGES: usize = 20;

/// The CRA repository: `provider:host:path` (env `DSYNC_CRA_REPO`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CraRepo {
    pub provider: ForgeProvider,
    /// Lowercase host.
    pub host: String,
    /// Lowercase repository path (`owner/repo`, `group/sub/project`).
    pub path: String,
}

impl CraRepo {
    pub fn new(provider: ForgeProvider, host: &str, path: &str) -> Self {
        CraRepo {
            provider,
            host: host.trim().to_ascii_lowercase(),
            path: path.trim().trim_matches('/').to_ascii_lowercase(),
        }
    }

    /// Parse `provider:host:path`. Any other shape is an error (fail closed at startup).
    pub fn parse(spec: &str) -> Result<Self, String> {
        let parts: Vec<&str> = spec.trim().split(':').map(str::trim).collect();
        if parts.len() != 3 {
            return Err(format!("DSYNC_CRA_REPO: '{spec}' is not provider:host:path"));
        }
        let provider = ForgeProvider::parse(parts[0])
            .ok_or_else(|| format!("DSYNC_CRA_REPO: unknown provider '{}'", parts[0]))?;
        let repo = CraRepo::new(provider, parts[1], parts[2]);
        if repo.host.is_empty() || repo.path.is_empty() || !repo.path.contains('/') {
            return Err(format!("DSYNC_CRA_REPO: '{spec}' needs a host and an owner/repo path"));
        }
        if repo.path.split('/').any(|s| s.is_empty() || s == "." || s == "..") {
            return Err(format!("DSYNC_CRA_REPO: '{spec}' has an invalid path"));
        }
        Ok(repo)
    }

    /// Is `(provider, host, repo)` this repository? Host and path compare case-insensitively.
    pub fn matches(&self, provider: ForgeProvider, host: &str, repo: &str) -> bool {
        provider == self.provider
            && host.trim().eq_ignore_ascii_case(&self.host)
            && repo.trim().trim_matches('/').eq_ignore_ascii_case(&self.path)
    }

    pub fn spec(&self) -> String {
        format!("{}:{}:{}", self.provider.as_str(), self.host, self.path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SourceError {
    /// Transport failure or a non-success HTTP status.
    #[error("forge API error: {0}")]
    Api(String),
    /// The API answered, but not in a shape that settles the question.
    #[error("ambiguous forge answer: {0}")]
    Ambiguous(String),
    /// The request itself cannot be answered (bad SHA, unknown pusher, file too large).
    #[error("{0}")]
    Invalid(String),
}

/// A CRA repository on a forge.
#[async_trait]
pub trait CraSource: Send + Sync {
    /// The repository this source reads.
    fn repo(&self) -> &CraRepo;

    /// The five `.dsync/` files at exactly `sha`.
    async fn fetch_files(&self, sha: &str) -> Result<CraFiles, SourceError>;

    /// Who made the change at `sha` and how it arrived. `pusher` is the
    /// authenticated pusher from a verified push webhook, when there is one.
    async fn change_context(
        &self,
        sha: &str,
        pusher: Option<&ForgeIdentity>,
        today: NaiveDate,
    ) -> Result<ChangeContext, SourceError>;

    /// The commit SHA at the head of `branch`.
    async fn branch_head(&self, branch: &str) -> Result<String, SourceError>;
}

/// A full lowercase hex commit id (SHA-1 or SHA-256). A branch or tag name is refused,
/// so files can only ever be read at an exact commit.
pub fn normalize_sha(sha: &str) -> Result<String, SourceError> {
    let s = sha.trim().to_ascii_lowercase();
    if (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit()) && s.bytes().any(|b| b != b'0') {
        Ok(s)
    } else {
        Err(SourceError::Invalid(format!("'{sha}' is not a full commit SHA")))
    }
}

/// RFC 3986 percent-encoding of everything except unreserved characters.
pub fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn cra_provider(p: ForgeProvider) -> super::ForgeProvider {
    match p {
        ForgeProvider::GitHub => super::ForgeProvider::Github,
        ForgeProvider::GitLab => super::ForgeProvider::Gitlab,
    }
}

fn identity(p: ForgeProvider, login: &str) -> ForgeIdentity {
    ForgeIdentity { provider: cra_provider(p), login: login.to_string() }
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

async fn read_text(resp: reqwest::Response, what: &str) -> Result<String, SourceError> {
    let status = resp.status();
    if !status.is_success() {
        return Err(SourceError::Api(format!("{what}: HTTP {}", status.as_u16())));
    }
    let bytes = resp.bytes().await.map_err(|e| SourceError::Api(format!("{what}: {e}")))?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(SourceError::Invalid(format!("{what}: larger than {MAX_FILE_BYTES} bytes")));
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| SourceError::Invalid(format!("{what}: not UTF-8")))
}

async fn read_json(resp: reqwest::Response, what: &str) -> Result<Value, SourceError> {
    let status = resp.status();
    if !status.is_success() {
        return Err(SourceError::Api(format!("{what}: HTTP {}", status.as_u16())));
    }
    resp.json::<Value>().await.map_err(|e| SourceError::Api(format!("{what}: invalid JSON: {e}")))
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for p in path {
        cur = cur.get(*p)?;
    }
    cur.as_str().filter(|s| !s.is_empty())
}

fn array<'a>(v: &'a Value, what: &str) -> Result<&'a Vec<Value>, SourceError> {
    v.as_array().ok_or_else(|| SourceError::Ambiguous(format!("{what}: expected a list")))
}

fn same_login(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// The pusher of a direct push. Without one the actor is unknown and the load is refused.
fn direct_push(provider: ForgeProvider, pusher: Option<&ForgeIdentity>, today: NaiveDate) -> Result<ChangeContext, SourceError> {
    let pusher = pusher.ok_or_else(|| {
        SourceError::Invalid(
            "the commit did not arrive through a merged request and no verified push webhook names its pusher".into(),
        )
    })?;
    if pusher.provider != cra_provider(provider) || pusher.login.trim().is_empty() {
        return Err(SourceError::Invalid(format!("pusher {pusher} is not a {provider} identity")));
    }
    Ok(ChangeContext { actor: pusher.clone(), via_pull_request: false, author: None, approvers: Vec::new(), today })
}

/// The merger must be the pusher when a verified webhook names one.
fn check_merger(provider: ForgeProvider, merger: &str, pusher: Option<&ForgeIdentity>) -> Result<(), SourceError> {
    if let Some(p) = pusher {
        if p.provider != cra_provider(provider) || !same_login(&p.login, merger) {
            return Err(SourceError::Ambiguous(format!(
                "the webhook pusher {p} is not the merger {provider}:{merger}"
            )));
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ GitHub

/// GitHub REST: contents at a ref, pull requests associated with a commit, reviews.
pub struct GitHubSource {
    repo: CraRepo,
    /// `https://api.github.com` for github.com; injectable for tests and GitHub Enterprise.
    api_base: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl GitHubSource {
    pub fn new(repo: CraRepo, api_base: Option<String>, token: Option<String>) -> Self {
        let api_base = api_base.unwrap_or_else(|| {
            if repo.host == "github.com" {
                "https://api.github.com".to_string()
            } else {
                format!("https://{}/api/v3", repo.host)
            }
        });
        GitHubSource {
            repo,
            api_base: api_base.trim_end_matches('/').to_string(),
            token: token.filter(|t| !t.trim().is_empty()),
            http: http_client(),
        }
    }

    fn get(&self, url: String, accept: &str) -> reqwest::RequestBuilder {
        let mut r = self
            .http
            .get(url)
            .header(USER_AGENT, USER_AGENT_VALUE)
            .header(ACCEPT, accept)
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(t) = &self.token {
            r = r.bearer_auth(t);
        }
        r
    }

    fn url(&self, rest: &str) -> String {
        format!("{}/repos/{}{}", self.api_base, self.repo.path, rest)
    }

    async fn json(&self, rest: &str, what: &str) -> Result<Value, SourceError> {
        let resp = self
            .get(self.url(rest), "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| SourceError::Api(format!("{what}: {e}")))?;
        read_json(resp, what).await
    }

    /// Every review of a pull request, oldest first, across pages.
    async fn reviews(&self, number: i64) -> Result<Vec<Value>, SourceError> {
        let mut all = Vec::new();
        for page in 1..=MAX_PAGES {
            let v = self
                .json(&format!("/pulls/{number}/reviews?per_page={PER_PAGE}&page={page}"), "reviews")
                .await?;
            let items = array(&v, "reviews")?;
            let n = items.len();
            all.extend(items.iter().cloned());
            if n < PER_PAGE {
                return Ok(all);
            }
        }
        Err(SourceError::Ambiguous(format!("more than {} reviews", PER_PAGE * MAX_PAGES)))
    }
}

/// GitHub approvers: per reviewer, the latest state-changing review (APPROVED,
/// CHANGES_REQUESTED or DISMISSED) decides. It counts only when it is APPROVED and was
/// given on the pull request's head commit. A review without a commit id never counts.
pub fn github_approvers(reviews: &[Value], head_sha: &str) -> Vec<String> {
    let mut latest: BTreeMap<String, (String, String, Option<String>)> = BTreeMap::new();
    for r in reviews {
        let Some(login) = str_at(r, &["user", "login"]) else { continue };
        let Some(state) = r.get("state").and_then(Value::as_str) else { continue };
        let state = state.to_ascii_uppercase();
        if !matches!(state.as_str(), "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED") {
            continue; // COMMENTED and PENDING do not change a reviewer's verdict
        }
        let commit = r.get("commit_id").and_then(Value::as_str).map(|s| s.to_ascii_lowercase());
        latest.insert(login.to_ascii_lowercase(), (login.to_string(), state, commit));
    }
    latest
        .into_values()
        .filter(|(_, state, commit)| state == "APPROVED" && commit.as_deref() == Some(head_sha))
        .map(|(login, _, _)| login)
        .collect()
}

#[async_trait]
impl CraSource for GitHubSource {
    fn repo(&self) -> &CraRepo {
        &self.repo
    }

    async fn fetch_files(&self, sha: &str) -> Result<CraFiles, SourceError> {
        let sha = normalize_sha(sha)?;
        let mut out = CraFiles::default();
        for kind in [CraFileKind::Org, CraFileKind::Roles, CraFileKind::Members, CraFileKind::Merit, CraFileKind::Payouts] {
            let name = kind.file_name();
            let url = self.url(&format!("/contents/.dsync/{name}?ref={sha}"));
            let resp = self
                .get(url, "application/vnd.github.raw+json")
                .send()
                .await
                .map_err(|e| SourceError::Api(format!(".dsync/{name}: {e}")))?;
            let text = read_text(resp, &format!(".dsync/{name}")).await?;
            match kind {
                CraFileKind::Org => out.org = text,
                CraFileKind::Roles => out.roles = text,
                CraFileKind::Members => out.members = text,
                CraFileKind::Merit => out.merit = text,
                CraFileKind::Payouts => out.payouts = text,
            }
        }
        Ok(out)
    }

    async fn change_context(&self, sha: &str, pusher: Option<&ForgeIdentity>, today: NaiveDate) -> Result<ChangeContext, SourceError> {
        let sha = normalize_sha(sha)?;
        let prs = self.json(&format!("/commits/{sha}/pulls?per_page={PER_PAGE}"), "pulls for commit").await?;
        let prs = array(&prs, "pulls for commit")?;
        let merged: Vec<&Value> = prs
            .iter()
            .filter(|p| {
                p.get("merged_at").map_or(false, |m| !m.is_null())
                    && p.get("merge_commit_sha").and_then(Value::as_str).map(|s| s.eq_ignore_ascii_case(&sha)).unwrap_or(false)
            })
            .collect();
        let number = match merged.as_slice() {
            [] => return direct_push(ForgeProvider::GitHub, pusher, today),
            [one] => one
                .get("number")
                .and_then(Value::as_i64)
                .ok_or_else(|| SourceError::Ambiguous("pull request without a number".into()))?,
            _ => return Err(SourceError::Ambiguous(format!("{} merged pull requests claim commit {sha}", merged.len()))),
        };

        // Re-read the pull request itself: the list form has no merged_by.
        let pr = self.json(&format!("/pulls/{number}"), "pull request").await?;
        let is_merged = pr.get("merged").and_then(Value::as_bool) == Some(true);
        let merge_sha_ok = pr.get("merge_commit_sha").and_then(Value::as_str).map(|s| s.eq_ignore_ascii_case(&sha)).unwrap_or(false);
        if !is_merged || !merge_sha_ok {
            return Err(SourceError::Ambiguous(format!("pull request #{number} is not merged as commit {sha}")));
        }
        let merger = str_at(&pr, &["merged_by", "login"])
            .ok_or_else(|| SourceError::Ambiguous(format!("pull request #{number} has no merged_by")))?;
        let author = str_at(&pr, &["user", "login"])
            .ok_or_else(|| SourceError::Ambiguous(format!("pull request #{number} has no author")))?;
        let head = str_at(&pr, &["head", "sha"])
            .map(str::to_ascii_lowercase)
            .ok_or_else(|| SourceError::Ambiguous(format!("pull request #{number} has no head commit")))?;
        check_merger(ForgeProvider::GitHub, merger, pusher)?;

        let reviews = self.reviews(number).await?;
        let approvers = github_approvers(&reviews, &head);
        Ok(ChangeContext {
            actor: identity(ForgeProvider::GitHub, merger),
            via_pull_request: true,
            author: Some(identity(ForgeProvider::GitHub, author)),
            approvers: approvers.iter().map(|a| identity(ForgeProvider::GitHub, a)).collect(),
            today,
        })
    }

    async fn branch_head(&self, branch: &str) -> Result<String, SourceError> {
        let v = self.json(&format!("/branches/{}", encode_component(branch)), "branch").await?;
        let sha = str_at(&v, &["commit", "sha"]).ok_or_else(|| SourceError::Ambiguous("branch without a head commit".into()))?;
        normalize_sha(sha)
    }
}

// ------------------------------------------------------------------ GitLab

/// GitLab REST v4: repository files, merge requests for a commit, approvals.
pub struct GitLabSource {
    repo: CraRepo,
    /// `https://<host>/api/v4` by default; injectable for tests.
    api_base: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl GitLabSource {
    pub fn new(repo: CraRepo, api_base: Option<String>, token: Option<String>) -> Self {
        let api_base = api_base.unwrap_or_else(|| format!("https://{}/api/v4", repo.host));
        GitLabSource {
            repo,
            api_base: api_base.trim_end_matches('/').to_string(),
            token: token.filter(|t| !t.trim().is_empty()),
            http: http_client(),
        }
    }

    fn url(&self, rest: &str) -> String {
        format!("{}/projects/{}{}", self.api_base, encode_component(&self.repo.path), rest)
    }

    fn get(&self, url: String) -> reqwest::RequestBuilder {
        let mut r = self.http.get(url).header(USER_AGENT, USER_AGENT_VALUE);
        if let Some(t) = &self.token {
            r = r.header("PRIVATE-TOKEN", t);
        }
        r
    }

    async fn json(&self, rest: &str, what: &str) -> Result<Value, SourceError> {
        let resp = self.get(self.url(rest)).send().await.map_err(|e| SourceError::Api(format!("{what}: {e}")))?;
        read_json(resp, what).await
    }

    async fn paged(&self, rest: &str, what: &str) -> Result<Vec<Value>, SourceError> {
        let sep = if rest.contains('?') { '&' } else { '?' };
        let mut all = Vec::new();
        for page in 1..=MAX_PAGES {
            let v = self.json(&format!("{rest}{sep}per_page={PER_PAGE}&page={page}"), what).await?;
            let items = array(&v, what)?;
            let n = items.len();
            all.extend(items.iter().cloned());
            if n < PER_PAGE {
                return Ok(all);
            }
        }
        Err(SourceError::Ambiguous(format!("{what}: more than {} entries", PER_PAGE * MAX_PAGES)))
    }
}

fn parse_time(v: &Value, key: &str) -> Option<DateTime<Utc>> {
    v.get(key)
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
}

/// GitLab approvers. GitLab's approvals API names who currently approves but not
/// when, so an approval counts only when all three agree:
///
/// - the user is in the current `approved_by` list (revoked or reset approvals are gone);
/// - the user's latest approve/unapprove system note is an approval;
/// - that note is strictly later than the newest diff version (the last push to the request).
///
/// Missing notes, versions or timestamps mean the approval does not count.
pub fn gitlab_approvers(approvals: &Value, notes: &[Value], versions: &[Value]) -> Vec<String> {
    let Some(last_push) = versions.iter().filter_map(|v| parse_time(v, "created_at")).max() else {
        return Vec::new();
    };
    let mut verdict: BTreeMap<String, (bool, Option<DateTime<Utc>>)> = BTreeMap::new();
    let mut ordered: Vec<&Value> = notes.iter().filter(|n| n.get("system").and_then(Value::as_bool) == Some(true)).collect();
    ordered.sort_by_key(|n| parse_time(n, "created_at"));
    for n in ordered {
        let Some(login) = str_at(n, &["author", "username"]) else { continue };
        let body = n.get("body").and_then(Value::as_str).unwrap_or("").trim().to_ascii_lowercase();
        let approved = if body.starts_with("approved this merge request") {
            true
        } else if body.starts_with("unapproved this merge request") {
            false
        } else {
            continue;
        };
        verdict.insert(login.to_ascii_lowercase(), (approved, parse_time(n, "created_at")));
    }
    let Some(list) = approvals.get("approved_by").and_then(Value::as_array) else { return Vec::new() };
    let mut out = Vec::new();
    for a in list {
        let Some(login) = str_at(a, &["user", "username"]) else { continue };
        match verdict.get(&login.to_ascii_lowercase()) {
            Some((true, Some(at))) if *at > last_push => out.push(login.to_string()),
            _ => {}
        }
    }
    out.sort_by_key(|l| l.to_ascii_lowercase());
    out.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    out
}

#[async_trait]
impl CraSource for GitLabSource {
    fn repo(&self) -> &CraRepo {
        &self.repo
    }

    async fn fetch_files(&self, sha: &str) -> Result<CraFiles, SourceError> {
        let sha = normalize_sha(sha)?;
        let mut out = CraFiles::default();
        for kind in [CraFileKind::Org, CraFileKind::Roles, CraFileKind::Members, CraFileKind::Merit, CraFileKind::Payouts] {
            let name = kind.file_name();
            let file = encode_component(&format!(".dsync/{name}"));
            let resp = self
                .get(self.url(&format!("/repository/files/{file}/raw?ref={sha}")))
                .send()
                .await
                .map_err(|e| SourceError::Api(format!(".dsync/{name}: {e}")))?;
            let text = read_text(resp, &format!(".dsync/{name}")).await?;
            match kind {
                CraFileKind::Org => out.org = text,
                CraFileKind::Roles => out.roles = text,
                CraFileKind::Members => out.members = text,
                CraFileKind::Merit => out.merit = text,
                CraFileKind::Payouts => out.payouts = text,
            }
        }
        Ok(out)
    }

    async fn change_context(&self, sha: &str, pusher: Option<&ForgeIdentity>, today: NaiveDate) -> Result<ChangeContext, SourceError> {
        let sha = normalize_sha(sha)?;
        let mrs = self.json(&format!("/repository/commits/{sha}/merge_requests"), "merge requests for commit").await?;
        let mrs = array(&mrs, "merge requests for commit")?;
        let merged: Vec<&Value> = mrs
            .iter()
            .filter(|m| {
                m.get("state").and_then(Value::as_str) == Some("merged")
                    && m.get("merge_commit_sha").and_then(Value::as_str).map(|s| s.eq_ignore_ascii_case(&sha)).unwrap_or(false)
            })
            .collect();
        let iid = match merged.as_slice() {
            [] => return direct_push(ForgeProvider::GitLab, pusher, today),
            [one] => one
                .get("iid")
                .and_then(Value::as_i64)
                .ok_or_else(|| SourceError::Ambiguous("merge request without an iid".into()))?,
            _ => return Err(SourceError::Ambiguous(format!("{} merged merge requests claim commit {sha}", merged.len()))),
        };

        let mr = self.json(&format!("/merge_requests/{iid}"), "merge request").await?;
        let state_ok = mr.get("state").and_then(Value::as_str) == Some("merged");
        let sha_ok = mr.get("merge_commit_sha").and_then(Value::as_str).map(|s| s.eq_ignore_ascii_case(&sha)).unwrap_or(false);
        if !state_ok || !sha_ok {
            return Err(SourceError::Ambiguous(format!("merge request !{iid} is not merged as commit {sha}")));
        }
        let merger = str_at(&mr, &["merge_user", "username"])
            .or_else(|| str_at(&mr, &["merged_by", "username"]))
            .ok_or_else(|| SourceError::Ambiguous(format!("merge request !{iid} has no merge user")))?;
        let author = str_at(&mr, &["author", "username"])
            .ok_or_else(|| SourceError::Ambiguous(format!("merge request !{iid} has no author")))?;
        check_merger(ForgeProvider::GitLab, merger, pusher)?;

        let approvals = self.json(&format!("/merge_requests/{iid}/approvals"), "approvals").await?;
        let versions = self.paged(&format!("/merge_requests/{iid}/versions"), "versions").await?;
        let notes = self.paged(&format!("/merge_requests/{iid}/notes?sort=asc&order_by=created_at"), "notes").await?;
        let approvers = gitlab_approvers(&approvals, &notes, &versions);
        Ok(ChangeContext {
            actor: identity(ForgeProvider::GitLab, merger),
            via_pull_request: true,
            author: Some(identity(ForgeProvider::GitLab, author)),
            approvers: approvers.iter().map(|a| identity(ForgeProvider::GitLab, a)).collect(),
            today,
        })
    }

    async fn branch_head(&self, branch: &str) -> Result<String, SourceError> {
        let v = self.json(&format!("/repository/branches/{}", encode_component(branch)), "branch").await?;
        let sha = str_at(&v, &["commit", "id"]).ok_or_else(|| SourceError::Ambiguous("branch without a head commit".into()))?;
        normalize_sha(sha)
    }
}
