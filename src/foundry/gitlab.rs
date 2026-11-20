//! GitLab webhooks: token check, payload types and normalization to [`FoundryEvent`].
//!
//! Merge request hooks map `open` -> PrOpened, `merge` -> PrMerged, `approved` ->
//! ReviewApproved. Every key is prefixed `gitlab:<host>:`.

use super::{host_from_url, provider_key, FoundryEvent, FoundryEventKind, FoundryProvider};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Constant-time check of `X-Gitlab-Token` against the configured secret.
/// An unset or empty secret rejects everything; so does a missing header.
/// Both sides are hashed first so the comparison does not leak the secret's length.
pub fn verify_token(secret: Option<&str>, header: Option<&str>) -> bool {
    let Some(secret) = secret.filter(|s| !s.is_empty()) else {
        return false;
    };
    let Some(provided) = header else { return false };
    let a = Sha256::digest(secret.as_bytes());
    let b = Sha256::digest(provided.as_bytes());
    a.as_slice().ct_eq(b.as_slice()).into()
}

/// `gitlab:<host>:delivery:<X-Gitlab-Event-UUID or Idempotency-Key>`. Unchanged by the MSync
/// rename (contract 12.1): a redelivery is recognized only if the stored key matches.
pub fn delivery_key(host: &str, delivery_id: &str) -> String {
    provider_key(
        FoundryProvider::GitLab,
        host,
        &format!("delivery:{}", delivery_id),
    )
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
pub fn normalize(
    payload: &GitLabWebhookPayload,
    delivery: Option<&str>,
) -> Result<Option<FoundryEvent>, String> {
    if payload.object_kind != "merge_request" {
        return Ok(None);
    }
    let attrs = payload
        .object_attributes
        .as_ref()
        .ok_or("merge_request hook without object_attributes")?;
    let kind = match attrs.action.as_deref() {
        Some("open") => FoundryEventKind::PrOpened,
        Some("merge") => FoundryEventKind::PrMerged,
        Some("approved") => FoundryEventKind::ReviewApproved,
        _ => return Ok(None),
    };
    let user = payload
        .user
        .as_ref()
        .filter(|u| !u.username.is_empty())
        .ok_or("merge_request hook without user")?;
    let project = payload
        .project
        .as_ref()
        .ok_or("merge_request hook without project")?;
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
        FoundryEventKind::PrOpened => format!("pr_opened:{}", k),
        FoundryEventKind::PrMerged => format!(
            "pr_merged:{}:{}",
            k,
            attrs.merge_commit_sha.as_deref().unwrap_or("")
        ),
        _ => format!("review_approved:{}:{}", k, user.username),
    };
    Ok(Some(FoundryEvent {
        provider: FoundryProvider::GitLab,
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
        effect_key: provider_key(FoundryProvider::GitLab, &host, &rest),
    }))
}

/// A GitLab `Push Hook`, as far as CRA loading needs it. Parsed only after the token
/// check. The CRA files themselves are never read from it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitLabPushPayload {
    pub object_kind: String,
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub after: String,
    #[serde(default)]
    pub user_username: Option<String>,
    pub project: GitLabProject,
}

/// Normalize a GitLab push hook to a [`FoundryEventKind::Push`] event (`base_branch` is
/// the pushed branch, `merge_sha` the head commit, `actor_login` the pushing user).
/// `Ok(None)`: not a branch push (tag, deletion, other hook). `Err`: a push hook
/// missing what it needs.
pub fn normalize_push(
    p: &GitLabPushPayload,
    delivery: Option<&str>,
) -> Result<Option<FoundryEvent>, String> {
    if p.object_kind != "push" {
        return Ok(None);
    }
    let Some(branch) = p.ref_name.strip_prefix("refs/heads/") else {
        return Ok(None);
    };
    let head = p.after.trim().to_ascii_lowercase();
    if head.is_empty() || head.bytes().all(|b| b == b'0') {
        return Ok(None);
    }
    let user = p
        .user_username
        .clone()
        .filter(|u| !u.is_empty())
        .ok_or("push hook without user_username")?;
    let host = host_from_url(&p.project.web_url).ok_or("project.web_url has no host")?;
    if p.project.path_with_namespace.trim().is_empty() {
        return Err("project.path_with_namespace is empty".into());
    }
    Ok(Some(FoundryEvent {
        provider: FoundryProvider::GitLab,
        host: host.clone(),
        repo: Some(p.project.path_with_namespace.clone()),
        kind: FoundryEventKind::Push,
        actor_login: user,
        subject_id: 0,
        subject_number: 0,
        title: String::new(),
        merge_sha: Some(head.clone()),
        base_branch: Some(branch.to_string()),
        delivery_id: delivery.map(str::to_string),
        author_login: None,
        body: None,
        effect_key: provider_key(FoundryProvider::GitLab, &host, &format!("push:{}", head)),
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn mr_hook(
        action: &str,
        user: (i64, &str),
        author_id: i64,
        path: &str,
        web_url: &str,
    ) -> serde_json::Value {
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

    fn norm(v: serde_json::Value) -> Option<FoundryEvent> {
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
        let open = norm(mr_hook(
            "open",
            (5, "carol"),
            5,
            "grp/proj",
            "https://gitlab.com/grp/proj",
        ))
        .unwrap();
        assert_eq!(open.kind, FoundryEventKind::PrOpened);
        assert_eq!(open.host, "gitlab.com");
        assert_eq!(open.author_login.as_deref(), Some("carol"));
        assert_eq!(open.effect_key, "gitlab:gitlab.com:pr_opened:id555");

        let merged = norm(mr_hook(
            "merge",
            (9, "maint"),
            5,
            "grp/proj",
            "https://Git.Example.org/grp/proj",
        ))
        .unwrap();
        assert_eq!(merged.kind, FoundryEventKind::PrMerged);
        assert_eq!(merged.author_login, None); // merger is not the author
        assert_eq!(
            merged.effect_key,
            "gitlab:git.example.org:pr_merged:id555:deadbeef"
        );

        let appr = norm(mr_hook(
            "approved",
            (9, "maint"),
            5,
            "grp/proj",
            "https://gitlab.com/grp/proj",
        ))
        .unwrap();
        assert_eq!(appr.kind, FoundryEventKind::ReviewApproved);
        assert_eq!(
            appr.effect_key,
            "gitlab:gitlab.com:review_approved:id555:maint"
        );

        assert_eq!(
            delivery_key("gitlab.com", "u-1"),
            "gitlab:gitlab.com:delivery:u-1"
        );
        assert!(norm(mr_hook(
            "update",
            (5, "carol"),
            5,
            "grp/proj",
            "https://gitlab.com/grp/proj"
        ))
        .is_none());
        let push: GitLabWebhookPayload =
            serde_json::from_value(serde_json::json!({"object_kind": "push"})).unwrap();
        assert!(normalize(&push, None).unwrap().is_none());
    }

    pub fn push_hook(
        branch_ref: &str,
        after: &str,
        user: &str,
        path: &str,
        web_url: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "object_kind": "push", "event_name": "push", "ref": branch_ref,
            "before": "1111111111111111111111111111111111111111", "after": after, "checkout_sha": after,
            "user_username": user, "user_name": "Some User",
            "project": {"path_with_namespace": path, "web_url": web_url, "default_branch": "main"},
            "commits": [{"id": after, "modified": [".msync/members.toml"]}]
        })
    }

    #[test]
    fn push_normalization() {
        let sha = "abcdef0123456789abcdef0123456789abcdef01";
        let p: GitLabPushPayload = serde_json::from_value(push_hook(
            "refs/heads/main",
            sha,
            "carol",
            "grp/cra",
            "https://GitLab.com/grp/cra",
        ))
        .unwrap();
        let ev = normalize_push(&p, Some("u-9")).unwrap().unwrap();
        assert_eq!(ev.kind, FoundryEventKind::Push);
        assert_eq!(
            (
                ev.host.as_str(),
                ev.repo.as_deref(),
                ev.base_branch.as_deref()
            ),
            ("gitlab.com", Some("grp/cra"), Some("main"))
        );
        assert_eq!(ev.merge_sha.as_deref(), Some(sha));
        assert_eq!(ev.actor_login, "carol");
        assert_eq!(ev.effect_key, format!("gitlab:gitlab.com:push:{sha}"));
        let tag: GitLabPushPayload = serde_json::from_value(push_hook(
            "refs/tags/v1",
            sha,
            "carol",
            "grp/cra",
            "https://gitlab.com/grp/cra",
        ))
        .unwrap();
        assert!(normalize_push(&tag, None).unwrap().is_none());
        let del: GitLabPushPayload = serde_json::from_value(push_hook(
            "refs/heads/main",
            "0000000000000000000000000000000000000000",
            "carol",
            "grp/cra",
            "https://gitlab.com/grp/cra",
        ))
        .unwrap();
        assert!(normalize_push(&del, None).unwrap().is_none());
        let mut anon = p;
        anon.user_username = None;
        assert!(normalize_push(&anon, None).is_err());
    }
}
