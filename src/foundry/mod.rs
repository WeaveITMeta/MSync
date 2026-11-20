//! Foundry API normalization (contract section 5): one event type from GitHub and GitLab
//! feeds merit and payouts.

pub mod github;
pub mod gitlab;
pub mod identity;
pub mod processor;
pub mod registry;

pub use processor::{FoundryProcessor, ProcessOutcome};
pub use registry::{RepoDecision, RepoEntry, RepoRegistry};

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FoundryProvider {
    GitHub,
    GitLab,
}

impl FoundryProvider {
    pub fn as_str(&self) -> &'static str {
        match self {
            FoundryProvider::GitHub => "github",
            FoundryProvider::GitLab => "gitlab",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "github" => Some(FoundryProvider::GitHub),
            "gitlab" => Some(FoundryProvider::GitLab),
            _ => None,
        }
    }

    pub fn default_host(&self) -> &'static str {
        match self {
            FoundryProvider::GitHub => "github.com",
            FoundryProvider::GitLab => "gitlab.com",
        }
    }
}

impl fmt::Display for FoundryProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FoundryEventKind {
    PrOpened,
    PrMerged,
    ReviewApproved,
    IssueClosed,
    ReleasePublished,
    Push,
}

/// Normalized Foundry API event. Fields after `delivery_id` are additions to the contract
/// shape that the merit path needs (see the field docs).
#[derive(Debug, Clone, PartialEq)]
pub struct FoundryEvent {
    pub provider: FoundryProvider,
    /// Lowercase host, e.g. `github.com`, `gitlab.example.org`.
    pub host: String,
    /// Repository path as GitHub or GitLab reports it (`org/repo`, `group/sub/project`).
    /// None when the payload carried no repository (legacy GitHub payloads).
    pub repo: Option<String>,
    pub kind: FoundryEventKind,
    /// Who performed the action (the reviewer for ReviewApproved).
    pub actor_login: String,
    /// Global id of the PR/MR (GitHub `pull_request.id`, GitLab `object_attributes.id`); 0 = unknown.
    pub subject_id: i64,
    /// PR number / MR iid; 0 = unknown.
    pub subject_number: i64,
    pub title: String,
    pub merge_sha: Option<String>,
    pub base_branch: Option<String>,
    pub delivery_id: Option<String>,
    /// Author of the PR/MR when the payload identifies them (merit for PR events goes here).
    pub author_login: Option<String>,
    /// PR/MR description.
    pub body: Option<String>,
    /// Once-only effect key for this event, in the provider's exact format.
    /// GitHub keys are byte-for-byte the pre-normalization keys; GitLab keys carry `gitlab:<host>:`.
    pub effect_key: String,
}

/// Key of a scorable subject: `(provider, host, repo, subject_id)`. Host and repo are
/// lowercased so `Org/Repo` and `org/repo` are the same subject.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Subject {
    pub provider: FoundryProvider,
    pub host: String,
    pub repo: String,
    pub subject_id: String,
}

impl Subject {
    pub fn new(provider: FoundryProvider, host: &str, repo: &str, subject_id: &str) -> Self {
        Self {
            provider,
            host: host.trim().to_ascii_lowercase(),
            repo: repo.trim().trim_matches('/').to_ascii_lowercase(),
            subject_id: subject_id.trim().to_string(),
        }
    }
}

/// A repository reference `(provider, host, repo)`, normalized like [`Subject`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RepoRef {
    pub provider: String,
    pub host: String,
    pub repo: String,
}

impl RepoRef {
    pub fn new(provider: FoundryProvider, host: &str, repo: &str) -> Self {
        Self {
            provider: provider.as_str().to_string(),
            host: host.trim().to_ascii_lowercase(),
            repo: repo.trim().trim_matches('/').to_ascii_lowercase(),
        }
    }
}

impl FoundryEvent {
    /// The scorable subject, when the repository and subject are known.
    pub fn subject(&self) -> Option<Subject> {
        let repo = self.repo.as_deref()?;
        let sid = subject_id_string(self.subject_id, self.subject_number)?;
        Some(Subject::new(self.provider, &self.host, repo, &sid))
    }

    pub fn repo_ref(&self) -> Option<RepoRef> {
        self.repo
            .as_deref()
            .map(|r| RepoRef::new(self.provider, &self.host, r))
    }
}

/// Subject id string: the global id, or `n<number>` when the id is unknown.
pub fn subject_id_string(subject_id: i64, subject_number: i64) -> Option<String> {
    if subject_id != 0 {
        Some(subject_id.to_string())
    } else if subject_number != 0 {
        Some(format!("n{}", subject_number))
    } else {
        None
    }
}

/// Prefix an effect/delivery key for its provider. GitHub keys are unprefixed
/// (their historical form); GitLab keys are `gitlab:<host>:<rest>`. These dedupe keys are
/// stored in `processed_webhook_events` and keep their exact bytes through the MSync rename
/// (contract 12.1), so events processed before it are still recognized.
pub fn provider_key(provider: FoundryProvider, host: &str, rest: &str) -> String {
    match provider {
        FoundryProvider::GitHub => rest.to_string(),
        FoundryProvider::GitLab => format!("gitlab:{}:{}", host.to_ascii_lowercase(), rest),
    }
}

/// Legacy `contributors.github_username` value for a GitHub or GitLab login. GitHub on
/// github.com is the bare login (today's form). Others carry a prefix with ':',
/// which no GitHub login can contain.
pub fn legacy_username(provider: FoundryProvider, host: &str, login: &str) -> String {
    let host = host.to_ascii_lowercase();
    match provider {
        FoundryProvider::GitHub if host == "github.com" => login.to_string(),
        FoundryProvider::GitHub => format!("github:{}:{}", host, login),
        FoundryProvider::GitLab if host == "gitlab.com" => format!("gitlab:{}", login),
        FoundryProvider::GitLab => format!("gitlab:{}:{}", host, login),
    }
}

/// Lowercase host of a URL like `https://gitlab.example.org/group/proj`.
pub fn host_from_url(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?; // drop userinfo
                                              // Port dropped: registry entries are ':'-separated, so a host is a bare name.
    let host = host
        .split(':')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pinned (contract 12.1): the webhook dedupe keys stored in `processed_webhook_events`
    /// keep their exact pre-rename bytes, so a redelivery of an event processed before the
    /// MSync rename is still recognized as a duplicate.
    #[test]
    fn webhook_dedupe_keys_survive_the_rename() {
        assert_eq!(
            github::delivery_key("72d3162e-cc78-11e3-81ab-4c9367dc0958"),
            "delivery:72d3162e-cc78-11e3-81ab-4c9367dc0958"
        );
        assert_eq!(
            gitlab::delivery_key("GitLab.com", "u-1"),
            "gitlab:gitlab.com:delivery:u-1"
        );
        assert_eq!(
            provider_key(FoundryProvider::GitHub, "github.com", "pr_merged:id9:abc"),
            "pr_merged:id9:abc"
        );
        assert_eq!(
            provider_key(
                FoundryProvider::GitLab,
                "Git.Example.org",
                "review_approved:id5:maint"
            ),
            "gitlab:git.example.org:review_approved:id5:maint"
        );
    }

    #[test]
    fn legacy_usernames() {
        assert_eq!(
            legacy_username(FoundryProvider::GitHub, "github.com", "alice"),
            "alice"
        );
        assert_eq!(
            legacy_username(FoundryProvider::GitLab, "gitlab.com", "alice"),
            "gitlab:alice"
        );
        assert_eq!(
            legacy_username(FoundryProvider::GitLab, "GitLab.Example.org", "alice"),
            "gitlab:gitlab.example.org:alice"
        );
    }

    #[test]
    fn hosts() {
        assert_eq!(
            host_from_url("https://GitLab.com/g/p").as_deref(),
            Some("gitlab.com")
        );
        assert_eq!(
            host_from_url("https://git.example.org:8443/g/p").as_deref(),
            Some("git.example.org")
        );
        assert_eq!(host_from_url("").as_deref(), None);
    }
}
