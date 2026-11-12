//! GitLab webhooks: token check, payload types and normalization to [`ForgeEvent`].
//!
//! Merge request hooks map `open` -> PrOpened, `merge` -> PrMerged, `approved` ->
//! ReviewApproved. Every key is prefixed `gitlab:<host>:`.

use super::{host_from_url, provider_key, ForgeEvent, ForgeEventKind, ForgeProvider};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Constant-time check of `X-Gitlab-Token` against the configured secret.
/// An unset or empty secret rejects everything; so does a missing header.
/// Both sides are hashed first so the comparison does not leak the secret's length.
pub fn verify_token(secret: Option<&str>, header: Option<&str>) -> bool {
    let Some(secret) = secret.filter(|s| !s.is_empty()) else { return false };
    let Some(provided) = header else { return false };
    let a = Sha256::digest(secret.as_bytes());
    let b = Sha256::digest(provided.as_bytes());
    a.as_slice().ct_eq(b.as_slice()).into()
}

/// `gitlab:<host>:delivery:<X-Gitlab-Event-UUID or Idempotency-Key>`.
pub fn delivery_key(host: &str, delivery_id: &str) -> String {
    provider_key(ForgeProvider::GitLab, host, &format!("delivery:{}", delivery_id))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitLabUser {
    #[serde(default)]
    pub id: i64,
    pub username: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitLabProject {
    pub path_with_namespace: String,
    pub web_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitLabMrAttributes {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub iid: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub merge_commit_sha: Option<String>,
    #[serde(default)]
    pub target_branch: Option<String>,
    #[serde(default)]
    pub author_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitLabWebhookPayload {
    pub object_kind: String,
    #[serde(default)]
    pub user: Option<GitLabUser>,
    #[serde(default)]
    pub project: Option<GitLabProject>,
    #[serde(default)]
    pub object_attributes: Option<GitLabMrAttributes>,
}

fn mr_key(a: &GitLabMrAttributes) -> String {
    if a.id != 0 {
        format!("id{}", a.id)
    } else {
        format!("n{}", a.iid)
    }
}

/// Normalize a GitLab hook. `Ok(None)`: not a merit event. `Err`: a merge request hook
/// missing what it needs (user, project, host).
pub fn normalize(payload: &GitLabWebhookPayload, delivery: Option<&str>) -> Result<Option<ForgeEvent>, String> {
    if payload.object_kind != "merge_request" {
        return Ok(None);
    }
    let attrs = payload.object_attributes.as_ref().ok_or("merge_request hook without object_attributes")?;
    let kind = match attrs.action.as_deref() {
        Some("open") => ForgeEventKind::PrOpened,
        Some("merge") => ForgeEventKind::PrMerged,
        Some("approved") => ForgeEventKind::ReviewApproved,
        _ => return Ok(None),
    };
    let user = payload.user.as_ref().filter(|u| !u.username.is_empty()).ok_or("merge_request hook without user")?;
    let project = payload.project.as_ref().ok_or("merge_request hook without project")?;
    let host = host_from_url(&project.web_url).ok_or("project.web_url has no host")?;
    if project.path_with_namespace.trim().is_empty() {
        return Err("project.path_with_namespace is empty".into());
    }
    // GitLab hooks name only the acting user; the author is known only when the
    // actor is the author (author_id matches). Otherwise the processor looks the
    // author up from the contribution recorded at open.
    let author_login = (user.id != 0 && user.id == attrs.author_id).then(|| user.username.clone());
    let k = mr_key(attrs);
    let rest = match kind {
        ForgeEventKind::PrOpened => format!("pr_opened:{}", k),
        ForgeEventKind::PrMerged => format!("pr_merged:{}:{}", k, attrs.merge_commit_sha.as_deref().unwrap_or("")),
        _ => format!("review_approved:{}:{}", k, user.username),
    };
    Ok(Some(ForgeEvent {
        provider: ForgeProvider::GitLab,
        host: host.clone(),
        repo: Some(project.path_with_namespace.clone()),
        kind,
        actor_login: user.username.clone(),
        subject_id: attrs.id,
        subject_number: attrs.iid,
        title: attrs.title.clone(),
        merge_sha: attrs.merge_commit_sha.clone().filter(|s| !s.is_empty()),
        base_branch: attrs.target_branch.clone(),
        delivery_id: delivery.map(str::to_string),
        author_login,
        body: attrs.description.clone(),
        effect_key: provider_key(ForgeProvider::GitLab, &host, &rest),
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn mr_hook(action: &str, user: (i64, &str), author_id: i64, path: &str, web_url: &str) -> serde_json::Value {
        serde_json::json!({
            "object_kind": "merge_request",
            "user": {"id": user.0, "username": user.1},
            "project": {"path_with_namespace": path, "web_url": web_url},
            "object_attributes": {
                "id": 555, "iid": 12, "title": "Add feature", "description": "why",
                "action": action, "state": if action == "merge" { "merged" } else { "opened" },
                "merge_commit_sha": if action == "merge" { Some("deadbeef") } else { None },
                "target_branch": "main", "author_id": author_id
            }
        })
    }

    fn norm(v: serde_json::Value) -> Option<ForgeEvent> {
        normalize(&serde_json::from_value(v).unwrap(), Some("u-1")).unwrap()
    }

    #[test]
    fn token_check() {
        assert!(verify_token(Some("s3cret"), Some("s3cret")));
        assert!(!verify_token(Some("s3cret"), Some("wrong")));
        assert!(!verify_token(Some("s3cret"), Some("")));
        assert!(!verify_token(Some("s3cret"), None));
        assert!(!verify_token(Some(""), Some("")));
        assert!(!verify_token(None, Some("anything")));
        assert!(!verify_token(None, None));
    }

    #[test]
    fn maps_actions_and_prefixes_keys() {
        let open = norm(mr_hook("open", (5, "carol"), 5, "grp/proj", "https://gitlab.com/grp/proj")).unwrap();
        assert_eq!(open.kind, ForgeEventKind::PrOpened);
        assert_eq!(open.host, "gitlab.com");
        assert_eq!(open.author_login.as_deref(), Some("carol"));
        assert_eq!(open.effect_key, "gitlab:gitlab.com:pr_opened:id555");

        let merged = norm(mr_hook("merge", (9, "maint"), 5, "grp/proj", "https://Git.Example.org/grp/proj")).unwrap();
        assert_eq!(merged.kind, ForgeEventKind::PrMerged);
        assert_eq!(merged.author_login, None); // merger is not the author
        assert_eq!(merged.effect_key, "gitlab:git.example.org:pr_merged:id555:deadbeef");

        let appr = norm(mr_hook("approved", (9, "maint"), 5, "grp/proj", "https://gitlab.com/grp/proj")).unwrap();
        assert_eq!(appr.kind, ForgeEventKind::ReviewApproved);
        assert_eq!(appr.effect_key, "gitlab:gitlab.com:review_approved:id555:maint");

        assert_eq!(delivery_key("gitlab.com", "u-1"), "gitlab:gitlab.com:delivery:u-1");
        assert!(norm(mr_hook("update", (5, "carol"), 5, "grp/proj", "https://gitlab.com/grp/proj")).is_none());
        let push: GitLabWebhookPayload = serde_json::from_value(serde_json::json!({"object_kind": "push"})).unwrap();
        assert!(normalize(&push, None).unwrap().is_none());
    }
}
