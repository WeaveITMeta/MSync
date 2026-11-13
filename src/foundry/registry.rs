//! Which repositories earn merit (contract 5 and 5.1).
//!
//! Round 1 source: `DSYNC_FOUNDRY_REPOS`, a comma list of `provider:host:path[:flag[:flag]]`
//! with flags `mirror` (earns nothing) and `private` (never named publicly).
//! Unset or empty: legacy mode. GitHub behaves as before (every repository earns and is
//! public) and GitLab is refused. A later round builds this from the CRA `[[foundry]]` list
//! with [`RepoRegistry::from_entries`].

use super::{FoundryProvider, RepoRef};
use serde::{Deserialize, Serialize};

pub const ENV_VAR: &str = "DSYNC_FOUNDRY_REPOS";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoEntry {
    pub provider: FoundryProvider,
    /// Lowercase host.
    pub host: String,
    /// Lowercase repository path.
    pub path: String,
    /// false for a mirror: listed, but earns nothing and creates no payout row.
    pub canonical: bool,
    pub private: bool,
}

impl RepoEntry {
    pub fn repo_ref(&self) -> RepoRef {
        RepoRef::new(self.provider, &self.host, &self.path)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoDecision {
    /// Earns merit and may create a payout row.
    Earn,
    /// Listed as a mirror: earns nothing.
    Mirror,
    /// Registry set and the repository is not listed (or the event has no repository).
    Unlisted,
    /// Registry unset and the provider is not GitHub.
    ProviderRefused,
}

/// Repository registry. `entries == None` is legacy mode (env var unset).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRegistry {
    entries: Option<Vec<RepoEntry>>,
}

impl RepoRegistry {
    /// Legacy mode: GitHub earns everywhere, GitLab refused.
    pub fn legacy() -> Self {
        Self { entries: None }
    }

    /// An explicit list (the CRA seam). An empty list means nothing earns.
    pub fn from_entries(entries: Vec<RepoEntry>) -> Self {
        Self { entries: Some(entries) }
    }

    pub fn from_env() -> Result<Self, String> {
        match std::env::var(ENV_VAR) {
            Ok(v) => Self::from_spec(&v),
            Err(_) => Ok(Self::legacy()),
        }
    }

    /// Parse `provider:host:path[:flag[:flag]]` entries. Blank spec = legacy mode.
    /// Any malformed entry is an error (fail closed at startup).
    pub fn from_spec(spec: &str) -> Result<Self, String> {
        if spec.trim().is_empty() {
            return Ok(Self::legacy());
        }
        let mut entries = Vec::new();
        for raw in spec.split(',') {
            let item = raw.trim();
            if item.is_empty() {
                continue;
            }
            let parts: Vec<&str> = item.split(':').map(str::trim).collect();
            if parts.len() < 3 || parts.len() > 5 {
                return Err(format!("{ENV_VAR}: '{item}' is not provider:host:path[:flag[:flag]]"));
            }
            let provider = FoundryProvider::parse(parts[0])
                .ok_or_else(|| format!("{ENV_VAR}: unknown provider '{}' in '{item}'", parts[0]))?;
            let host = parts[1].to_ascii_lowercase();
            let path = parts[2].trim_matches('/').to_ascii_lowercase();
            if host.is_empty() || path.is_empty() || !path.contains('/') {
                return Err(format!("{ENV_VAR}: '{item}' needs a host and an owner/repo path"));
            }
            let (mut canonical, mut private) = (true, false);
            for flag in &parts[3..] {
                match flag.to_ascii_lowercase().as_str() {
                    "mirror" if canonical => canonical = false,
                    "private" if !private => private = true,
                    other => return Err(format!("{ENV_VAR}: bad or repeated flag '{other}' in '{item}'")),
                }
            }
            let entry = RepoEntry { provider, host, path, canonical, private };
            if entries.iter().any(|e: &RepoEntry| e.repo_ref() == entry.repo_ref()) {
                return Err(format!("{ENV_VAR}: '{item}' listed twice"));
            }
            entries.push(entry);
        }
        Ok(Self { entries: Some(entries) })
    }

    pub fn is_legacy(&self) -> bool {
        self.entries.is_none()
    }

    pub fn entries(&self) -> &[RepoEntry] {
        self.entries.as_deref().unwrap_or(&[])
    }

    fn find(&self, provider: FoundryProvider, host: &str, repo: &str) -> Option<&RepoEntry> {
        let r = RepoRef::new(provider, host, repo);
        self.entries().iter().find(|e| e.repo_ref() == r)
    }

    /// May an event from this repository earn merit?
    pub fn decide(&self, provider: FoundryProvider, host: &str, repo: Option<&str>) -> RepoDecision {
        if self.is_legacy() {
            return match provider {
                FoundryProvider::GitHub => RepoDecision::Earn,
                _ => RepoDecision::ProviderRefused,
            };
        }
        let Some(repo) = repo else { return RepoDecision::Unlisted };
        match self.find(provider, host, repo) {
            Some(e) if e.canonical => RepoDecision::Earn,
            Some(_) => RepoDecision::Mirror,
            None => RepoDecision::Unlisted,
        }
    }

    /// May this repository be named on a public response? Legacy mode: yes (today's
    /// behaviour, GitHub only). Registry set: only a listed repository without `private`.
    /// An unlisted repository (for example one removed from the list) is treated as
    /// private, so a name is never leaked by omission.
    pub fn is_public(&self, r: &RepoRef) -> bool {
        if self.is_legacy() {
            return true;
        }
        let Some(provider) = FoundryProvider::parse(&r.provider) else { return false };
        matches!(self.find(provider, &r.host, &r.repo), Some(e) if !e.private)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use FoundryProvider::*;

    #[test]
    fn legacy_mode() {
        let r = RepoRegistry::from_spec("  ").unwrap();
        assert!(r.is_legacy());
        assert_eq!(r.decide(GitHub, "github.com", Some("a/b")), RepoDecision::Earn);
        assert_eq!(r.decide(GitHub, "github.com", None), RepoDecision::Earn);
        assert_eq!(r.decide(GitLab, "gitlab.com", Some("a/b")), RepoDecision::ProviderRefused);
    }

    #[test]
    fn listed_mirror_unlisted_private() {
        let r = RepoRegistry::from_spec(
            "github:github.com:Org/Main, gitlab:gitlab.com:org/main:mirror, gitlab:git.example.org:grp/sub/proj:private, github:github.com:org/secret:private:mirror",
        )
        .unwrap();
        assert_eq!(r.decide(GitHub, "github.com", Some("org/main")), RepoDecision::Earn);
        assert_eq!(r.decide(GitHub, "GITHUB.com", Some("ORG/MAIN")), RepoDecision::Earn);
        assert_eq!(r.decide(GitLab, "gitlab.com", Some("org/main")), RepoDecision::Mirror);
        assert_eq!(r.decide(GitLab, "git.example.org", Some("grp/sub/proj")), RepoDecision::Earn);
        assert_eq!(r.decide(GitHub, "github.com", Some("org/other")), RepoDecision::Unlisted);
        assert_eq!(r.decide(GitHub, "github.com", None), RepoDecision::Unlisted);
        assert_eq!(r.decide(GitHub, "github.com", Some("org/secret")), RepoDecision::Mirror);
        assert!(r.is_public(&RepoRef::new(GitHub, "github.com", "org/main")));
        assert!(!r.is_public(&RepoRef::new(GitLab, "git.example.org", "grp/sub/proj")));
        assert!(!r.is_public(&RepoRef::new(GitHub, "github.com", "org/unlisted")));
    }

    #[test]
    fn malformed_specs_fail_closed() {
        for bad in [
            "github:github.com",
            "bitbucket:bitbucket.org:a/b",
            "github::a/b",
            "github:github.com:noslash",
            "github:github.com:a/b:canonical",
            "github:github.com:a/b:mirror:mirror",
            "github:github.com:a/b,github:github.com:A/B",
        ] {
            assert!(RepoRegistry::from_spec(bad).is_err(), "{bad} should be rejected");
        }
    }
}
