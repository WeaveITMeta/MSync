//! GitHub payload -> [`ForgeEvent`]. Dedupe keys keep their exact pre-normalization form.

use super::{ForgeEvent, ForgeEventKind, ForgeProvider};
use crate::github::webhook::{GitHubWebhookPayload, PullRequestPayload};

/// Only github.com is supported by the GitHub webhook today.
pub const HOST: &str = "github.com";

/// `delivery:<X-GitHub-Delivery>` (unchanged).
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

fn base(pr: &PullRequestPayload, payload: &GitHubWebhookPayload, kind: ForgeEventKind, actor: String, key: String, delivery: Option<&str>) -> ForgeEvent {
    ForgeEvent {
        provider: ForgeProvider::GitHub,
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
pub fn normalize(event_type: &str, payload: &GitHubWebhookPayload, delivery: Option<&str>) -> Option<ForgeEvent> {
    match event_type {
        "pull_request" => {
            let pr = payload.pull_request.as_ref()?;
            let actor = pr.user.login.clone();
            match payload.action.as_deref().unwrap_or("") {
                "opened" => Some(base(pr, payload, ForgeEventKind::PrOpened, actor, pr_opened_key(pr), delivery)),
                "closed" if pr.merged => Some(base(pr, payload, ForgeEventKind::PrMerged, actor, pr_merged_key(pr), delivery)),
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
                Some(pr) => base(pr, payload, ForgeEventKind::ReviewApproved, reviewer, key, delivery),
                None => ForgeEvent {
                    provider: ForgeProvider::GitHub,
                    host: HOST.to_string(),
                    repo: payload.repository.as_ref().map(|r| r.full_name.clone()),
                    kind: ForgeEventKind::ReviewApproved,
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
}
