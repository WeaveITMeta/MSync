//! Forge normalization (contract section 5): one event type from GitHub and GitLab
//! feeds merit and payouts.

pub mod github;
pub mod gitlab;
pub mod identity;
pub mod processor;
pub mod registry;

pub use processor::{ForgeProcessor, ProcessOutcome};
pub use registry::{RepoDecision, RepoEntry, RepoRegistry};

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForgeProvider {
    GitHub,
    GitLab,
}

impl ForgeProvider {
    pub fn as_str(&self) -> &'static str {
        match self {
            ForgeProvider::GitHub => "github",
            ForgeProvider::GitLab => "gitlab",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "github" => Some(ForgeProvider::GitHub),
            "gitlab" => Some(ForgeProvider::GitLab),
            _ => None,
        }
    }

    pub fn default_host(&self) -> &'static str {
        match self {
            ForgeProvider::GitHub => "github.com",
            ForgeProvider::GitLab => "gitlab.com",
        }
    }
}

impl fmt::Display for ForgeProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForgeEventKind {
    PrOpened,
    PrMerged,
    ReviewApproved,
    IssueClosed,
    ReleasePublished,
    Push,
}

/// Normalized forge event. Fields after `delivery_id` are additions to the contract
/// shape that the merit path needs (see the field docs).
#[derive(Debug, Clone, PartialEq)]
pub struct ForgeEvent {
    pub provider: ForgeProvider,
    /// Lowercase host, e.g. `github.com`, `gitlab.example.org`.
    pub host: String,
    /// Repository path as the forge reports it (`org/repo`, `group/sub/project`).
    /// None when the payload carried no repository (legacy GitHub payloads).
    pub repo: Option<String>,
    pub kind: ForgeEventKind,
    /// Who performed the action (the reviewer for ReviewApproved).
    pub actor_login: String,
    /// Forge-global id of the PR/MR (GitHub `pull_request.id`, GitLab `object_attributes.id`); 0 = unknown.
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
    pub provider: ForgeProvider,
    pub host: String,
    pub repo: String,
    pub subject_id: String,
}

impl Subject {
    pub fn new(provider: ForgeProvider, host: &str, repo: &str, subject_id: &str) -> Self {
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
    pub fn new(provider: ForgeProvider, host: &str, repo: &str) -> Self {
        Self {
            provider: provider.as_str().to_string(),
            host: host.trim().to_ascii_lowercase(),
            repo: repo.trim().trim_matches('/').to_ascii_lowercase(),
        }
    }
}

impl ForgeEvent {
    /// The scorable subject, when the repository and subject are known.
    pub fn subject(&self) -> Option<Subject> {
        let repo = self.repo.as_deref()?;
        let sid = subject_id_string(self.subject_id, self.subject_number)?;
        Some(Subject::new(self.provider, &self.host, repo, &sid))
    }

    pub fn repo_ref(&self) -> Option<RepoRef> {
        self.repo.as_deref().map(|r| RepoRef::new(self.provider, &self.host, r))
    }
}

/// Subject id string: the forge-global id, or `n<number>` when the id is unknown.
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
/// (their historical form); GitLab keys are `gitlab:<host>:<rest>`.
pub fn provider_key(provider: ForgeProvider, host: &str, rest: &str) -> String {
    match provider {
        ForgeProvider::GitHub => rest.to_string(),
        ForgeProvider::GitLab => format!("gitlab:{}:{}", host.to_ascii_lowercase(), rest),
    }
}

/// Legacy `contributors.github_username` value for a forge login. GitHub on
/// github.com is the bare login (today's form). Others carry a prefix with ':',
/// which no GitHub login can contain.
pub fn legacy_username(provider: ForgeProvider, host: &str, login: &str) -> String {
    let host = host.to_ascii_lowercase();
    match provider {
        ForgeProvider::GitHub if host == "github.com" => login.to_string(),
        ForgeProvider::GitHub => format!("github:{}:{}", host, login),
        ForgeProvider::GitLab if host == "gitlab.com" => format!("gitlab:{}", login),
        ForgeProvider::GitLab => format!("gitlab:{}:{}", host, login),
    }
}

/// Lowercase host of a URL like `https://gitlab.example.org/group/proj`.
pub fn host_from_url(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?; // drop userinfo
    // Port dropped: registry entries are ':'-separated, so a host is a bare name.
    let host = host.split(':').next().unwrap_or("").trim().to_ascii_lowercase();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_usernames() {
        assert_eq!(legacy_username(ForgeProvider::GitHub, "github.com", "alice"), "alice");
        assert_eq!(legacy_username(ForgeProvider::GitLab, "gitlab.com", "alice"), "gitlab:alice");
        assert_eq!(legacy_username(ForgeProvider::GitLab, "GitLab.Example.org", "alice"), "gitlab:gitlab.example.org:alice");
    }

    #[test]
    fn hosts() {
        assert_eq!(host_from_url("https://GitLab.com/g/p").as_deref(), Some("gitlab.com"));
        assert_eq!(host_from_url("https://git.example.org:8443/g/p").as_deref(), Some("git.example.org"));
        assert_eq!(host_from_url("").as_deref(), None);
    }
}
