//! GitHub payload -> [`FoundryEvent`]. Dedupe keys keep their exact pre-normalization form.

use super::{FoundryEvent, FoundryEventKind, FoundryProvider};
use crate::github::webhook::{GitHubUser, GitHubWebhookPayload, PullRequestPayload, RepositoryPayload};
use serde::Deserialize;

/// Only github.com is supported by the GitHub webhook today.
pub const HOST: &str = "github.com";

/// `delivery:<X-GitHub-Delivery>` (unchanged). Dedupe keys keep their exact bytes through the
/// MSync rename (contract 12.1): a redelivery is recognized only if the stored key matches.
pub fn delivery_key(delivery_id: &str) -> String {
    format!("delivery:{}", delivery_id)
}

/// Stable identity for a PR: GitHub's global id when present, else the number (unchanged).
pub fn pr_key(pr: &PullRequestPayload) -> String {
    if pr.id != 0 {
        format!("id{}", pr.id)
    } else {
        format!("n{}", pr.number)
    }
}

pub fn pr_opened_key(pr: &PullRequestPayload) -> String {
    format!("pr_opened:{}", pr_key(pr))
}

pub fn pr_merged_key(pr: &PullRequestPayload) -> String {
    format!("pr_merged:{}:{}", pr_key(pr), pr.merge_commit_sha.as_deref().unwrap_or(""))
}

pub fn review_approved_key(pr: Option<&PullRequestPayload>, reviewer: &str) -> String {
    let pr_part = pr.map(pr_key).unwrap_or_else(|| "unknown".into());
    format!("review_approved:{}:{}", pr_part, reviewer)
}

fn base(pr: &PullRequestPayload, payload: &GitHubWebhookPayload, kind: FoundryEventKind, actor: String, key: String, delivery: Option<&str>) -> FoundryEvent {
    FoundryEvent {
        provider: FoundryProvider::GitHub,
        host: HOST.to_string(),
        repo: payload.repository.as_ref().map(|r| r.full_name.clone()),
        kind,
        actor_login: actor,
        subject_id: pr.id,
        subject_number: pr.number as i64,
        title: pr.title.clone(),
        merge_sha: pr.merge_commit_sha.clone(),
        base_branch: pr.base.as_ref().map(|b| b.ref_name.clone()),
        delivery_id: delivery.map(str::to_string),
        author_login: Some(pr.user.login.clone()),
        body: pr.body.clone(),
        effect_key: key,
    }
}

/// Normalize a GitHub webhook. Returns None for events that carry no merit effect.
pub fn normalize(event_type: &str, payload: &GitHubWebhookPayload, delivery: Option<&str>) -> Option<FoundryEvent> {
    match event_type {
        "pull_request" => {
            let pr = payload.pull_request.as_ref()?;
            let actor = pr.user.login.clone();
            match payload.action.as_deref().unwrap_or("") {
                "opened" => Some(base(pr, payload, FoundryEventKind::PrOpened, actor, pr_opened_key(pr), delivery)),
                "closed" if pr.merged => Some(base(pr, payload, FoundryEventKind::PrMerged, actor, pr_merged_key(pr), delivery)),
                _ => None,
            }
        }
        "pull_request_review" => {
            let review = payload.review.as_ref()?;
            if review.state != "approved" {
                return None;
            }
            let reviewer = review.user.login.clone();
            let key = review_approved_key(payload.pull_request.as_ref(), &reviewer);
            Some(match payload.pull_request.as_ref() {
                Some(pr) => base(pr, payload, FoundryEventKind::ReviewApproved, reviewer, key, delivery),
                None => FoundryEvent {
                    provider: FoundryProvider::GitHub,
                    host: HOST.to_string(),
                    repo: payload.repository.as_ref().map(|r| r.full_name.clone()),
                    kind: FoundryEventKind::ReviewApproved,
                    actor_login: reviewer,
                    subject_id: 0,
                    subject_number: 0,
                    title: String::new(),
                    merge_sha: None,
                    base_branch: None,
                    delivery_id: delivery.map(str::to_string),
                    author_login: None,
                    body: None,
                    effect_key: key,
                },
            })
        }
        _ => None,
    }
}

/// A GitHub `push` webhook, as far as CRA loading needs it. Parsed only after the
/// signature check. The body names the pushed branch, the head commit and the
/// authenticated sender; the CRA files themselves are never read from it.
#[derive(Debug, Clone, Deserialize)]
pub struct GitHubPushPayload {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub after: String,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub repository: Option<RepositoryPayload>,
    #[serde(default)]
    pub sender: Option<GitHubUser>,
}

/// Normalize a GitHub `push` to a [`FoundryEventKind::Push`] event: `base_branch` is the
/// pushed branch, `merge_sha` the pushed head commit, `actor_login` the authenticated
/// sender. Tags, branch deletions and pushes without a sender or repository are None.
pub fn normalize_push(p: &GitHubPushPayload, delivery: Option<&str>) -> Option<FoundryEvent> {
    let branch = p.ref_name.strip_prefix("refs/heads/")?.to_string();
    let head = p.after.trim().to_ascii_lowercase();
    if p.deleted || head.is_empty() || head.bytes().all(|b| b == b'0') {
        return None;
    }
    let sender = p.sender.as_ref().map(|s| s.login.clone()).filter(|l| !l.is_empty())?;
    let repo = p.repository.as_ref().map(|r| r.full_name.clone()).filter(|r| !r.is_empty())?;
    Some(FoundryEvent {
        provider: FoundryProvider::GitHub,
        host: HOST.to_string(),
        repo: Some(repo),
        kind: FoundryEventKind::Push,
        actor_login: sender,
        subject_id: 0,
        subject_number: 0,
        title: String::new(),
        merge_sha: Some(head.clone()),
        base_branch: Some(branch),
        delivery_id: delivery.map(str::to_string),
        author_login: None,
        body: None,
        effect_key: format!("push:{}", head),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(v: serde_json::Value) -> GitHubWebhookPayload {
        serde_json::from_value(v).unwrap()
    }

    fn pr(id: i64, sha: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "id": id, "number": 7, "title": "T", "body": null, "state": "closed", "merged": true,
            "user": {"login": "alice"}, "merge_commit_sha": sha, "base": {"ref": "main"}
        })
    }

    /// The literal key strings must never change: redeliveries of events processed
    /// before normalization are recognized only if these bytes match.
    #[test]
    fn github_dedupe_key_literals_unchanged() {
        assert_eq!(delivery_key("72d3162e-cc78-11e3-81ab-4c9367dc0958"), "delivery:72d3162e-cc78-11e3-81ab-4c9367dc0958");

        let open = payload(serde_json::json!({"action": "opened", "pull_request": pr(9001, None)}));
        assert_eq!(normalize("pull_request", &open, None).unwrap().effect_key, "pr_opened:id9001");
        let open_noid = payload(serde_json::json!({"action": "opened", "pull_request": pr(0, None)}));
        assert_eq!(normalize("pull_request", &open_noid, None).unwrap().effect_key, "pr_opened:n7");

        let merged = payload(serde_json::json!({"action": "closed", "pull_request": pr(9001, Some("abc123"))}));
        assert_eq!(normalize("pull_request", &merged, None).unwrap().effect_key, "pr_merged:id9001:abc123");
        let merged_nosha = payload(serde_json::json!({"action": "closed", "pull_request": pr(9001, None)}));
        assert_eq!(normalize("pull_request", &merged_nosha, None).unwrap().effect_key, "pr_merged:id9001:");

        let review = payload(serde_json::json!({
            "action": "submitted", "pull_request": pr(9001, None),
            "review": {"state": "approved", "body": null, "user": {"login": "bob"}}
        }));
        assert_eq!(normalize("pull_request_review", &review, None).unwrap().effect_key, "review_approved:id9001:bob");
        let review_nopr = payload(serde_json::json!({
            "action": "submitted", "review": {"state": "approved", "body": null, "user": {"login": "bob"}}
        }));
        assert_eq!(normalize("pull_request_review", &review_nopr, None).unwrap().effect_key, "review_approved:unknown:bob");
    }

    #[test]
    fn non_merit_events_are_none() {
        let closed_unmerged = payload(serde_json::json!({"action": "closed", "pull_request": {
            "id": 1, "number": 1, "title": "T", "body": null, "state": "closed", "merged": false, "user": {"login": "a"}}}));
        assert!(normalize("pull_request", &closed_unmerged, None).is_none());
        let commented = payload(serde_json::json!({"action": "submitted",
            "review": {"state": "commented", "body": null, "user": {"login": "bob"}}}));
        assert!(normalize("pull_request_review", &commented, None).is_none());
        assert!(normalize("push", &commented, None).is_none());
    }

    #[test]
    fn push_normalization() {
        let p: GitHubPushPayload = serde_json::from_value(serde_json::json!({
            "ref": "refs/heads/main", "after": "ABCDEF0123456789abcdef0123456789abcdef01", "deleted": false,
            "repository": {"full_name": "Org/CRA"}, "sender": {"login": "alice"}, "pusher": {"name": "alice"}
        })).unwrap();
        let ev = normalize_push(&p, Some("d-1")).unwrap();
        assert_eq!(ev.kind, FoundryEventKind::Push);
        assert_eq!(ev.base_branch.as_deref(), Some("main"));
        assert_eq!(ev.merge_sha.as_deref(), Some("abcdef0123456789abcdef0123456789abcdef01"));
        assert_eq!(ev.actor_login, "alice");
        assert_eq!(ev.repo.as_deref(), Some("Org/CRA"));
        // A push never feeds the merit path's own normalizer.
        let as_merit: GitHubWebhookPayload = serde_json::from_value(serde_json::json!({"repository": {"full_name": "Org/CRA"}})).unwrap();
        assert!(normalize("push", &as_merit, None).is_none());

        let mut tag = p.clone();
        tag.ref_name = "refs/tags/v1".into();
        assert!(normalize_push(&tag, None).is_none());
        let mut del = p.clone();
        del.deleted = true;
        assert!(normalize_push(&del, None).is_none());
        let mut zero = p.clone();
        zero.after = "0000000000000000000000000000000000000000".into();
        assert!(normalize_push(&zero, None).is_none());
        let mut anon = p;
        anon.sender = None;
        assert!(normalize_push(&anon, None).is_none());
    }
}
