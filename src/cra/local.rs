//! A local directory standing in for the CRA repository (contract 13.5), for development
//! only: `MSYNC_CRA_LOCAL_DIR=<dir containing .msync/>` with `MSYNC_DEV_MODE=true`.
//!
//! It goes through the same path as a real push: [`super::loader::load_commit`] parses,
//! validates, plans against the active snapshot and authorizes. What differs is where the
//! facts come from:
//!
//! - The "commit" is the content hash of the five files (64 hex characters), so the same
//!   files are the same commit and a reload of unchanged files is idempotent.
//! - [`LocalDirSource::fetch_files`] rereads the directory and refuses when the files no
//!   longer hash to the requested commit (they changed between head and fetch).
//! - The actor is the first configured owner of the form `provider:login`
//!   (`MSYNC_OWNER_MEMBER_IDS`), as a direct push: no pull request, no approvers. So the
//!   bootstrap is accepted for an owner, and a later two-person change is refused exactly as
//!   a direct push to a real repository would be.
//! - The repository is `github:local.invalid:local/<dir name>`. No webhook can name that
//!   host, so a push webhook can never trigger a local load.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::NaiveDate;

use super::source::{CraRepo, CraSource, SourceError, MAX_FILE_BYTES};
use super::{ChangeContext, CraFileKind, CraFiles, FoundryIdentity, FoundryProvider as CraProvider};
use crate::foundry::FoundryProvider;

/// The host of every local CRA repository. `.invalid` is reserved (RFC 2606).
pub const LOCAL_HOST: &str = "local.invalid";
/// The branch name a local CRA reports.
pub const LOCAL_BRANCH: &str = "local";

/// Read the five `.msync/` files of `dir`. Every missing, oversized or non-UTF-8 file is
/// an error naming the file.
pub fn read_dir_files(dir: &Path) -> Result<CraFiles, Vec<String>> {
    let base = dir.join(".msync");
    let mut errors = Vec::new();
    let mut read = |kind: CraFileKind| -> String {
        let path = base.join(kind.file_name());
        match std::fs::read(&path) {
            Ok(bytes) if bytes.len() > MAX_FILE_BYTES => {
                errors.push(format!("{}: larger than {MAX_FILE_BYTES} bytes", path.display()));
                String::new()
            }
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => {
                    errors.push(format!("{}: not UTF-8", path.display()));
                    String::new()
                }
            },
            Err(e) => {
                errors.push(format!("{}: {e}", path.display()));
                String::new()
            }
        }
    };
    let files = CraFiles {
        org: read(CraFileKind::Org),
        roles: read(CraFileKind::Roles),
        members: read(CraFileKind::Members),
        merit: read(CraFileKind::Merit),
        payouts: read(CraFileKind::Payouts),
    };
    if errors.is_empty() {
        Ok(files)
    } else {
        Err(errors)
    }
}

/// The first owner entry of the form `provider:login`, as the actor of a local load.
pub fn local_actor(owners: &[String]) -> Option<FoundryIdentity> {
    owners.iter().find_map(|o| {
        let (p, login) = o.split_once(':')?;
        let login = login.trim();
        if login.is_empty() {
            return None;
        }
        match p.trim().to_ascii_lowercase().as_str() {
            "github" => Some(FoundryIdentity { provider: CraProvider::Github, login: login.to_string() }),
            "gitlab" => Some(FoundryIdentity { provider: CraProvider::Gitlab, login: login.to_string() }),
            _ => None,
        }
    })
}

/// The pseudo repository of a local directory.
pub fn local_repo(dir: &Path) -> CraRepo {
    let name: String = dir
        .canonicalize()
        .unwrap_or_else(|_| dir.to_path_buf())
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' })
        .collect();
    let name = if name.trim_matches('.').is_empty() { "cra".to_string() } else { name };
    CraRepo::new(FoundryProvider::GitHub, LOCAL_HOST, &format!("local/{name}"))
}

pub struct LocalDirSource {
    repo: CraRepo,
    dir: PathBuf,
    actor: FoundryIdentity,
}

impl LocalDirSource {
    /// `None` when no owner entry has the `provider:login` form (there would be no actor).
    pub fn new(dir: PathBuf, owners: &[String]) -> Option<Self> {
        let actor = local_actor(owners)?;
        Some(LocalDirSource { repo: local_repo(&dir), dir, actor })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn actor(&self) -> &FoundryIdentity {
        &self.actor
    }

    fn read(&self) -> Result<CraFiles, SourceError> {
        read_dir_files(&self.dir).map_err(|e| SourceError::Invalid(e.join("; ")))
    }
}

#[async_trait]
impl CraSource for LocalDirSource {
    fn repo(&self) -> &CraRepo {
        &self.repo
    }

    async fn fetch_files(&self, sha: &str) -> Result<CraFiles, SourceError> {
        let files = self.read()?;
        if files.content_hash() != sha.trim().to_ascii_lowercase() {
            return Err(SourceError::Ambiguous(
                "the local .msync/ files changed while they were being loaded; reload again".into(),
            ));
        }
        Ok(files)
    }

    async fn change_context(
        &self,
        _sha: &str,
        _pusher: Option<&FoundryIdentity>,
        today: NaiveDate,
    ) -> Result<ChangeContext, SourceError> {
        Ok(ChangeContext { actor: self.actor.clone(), via_pull_request: false, author: None, approvers: Vec::new(), today })
    }

    async fn branch_head(&self, _branch: &str) -> Result<String, SourceError> {
        Ok(self.read()?.content_hash())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/cra/eustress-engine")
    }

    #[test]
    fn the_actor_is_the_first_provider_owner() {
        let owners = vec!["mckale".to_string(), "github:example-founder".to_string(), "gitlab:other".to_string()];
        assert_eq!(local_actor(&owners), Some(FoundryIdentity::github("example-founder")));
        assert_eq!(local_actor(&["mckale".to_string()]), None);
        assert_eq!(local_actor(&["github:".to_string()]), None);
        assert!(LocalDirSource::new(example_dir(), &["mckale".to_string()]).is_none());
    }

    #[test]
    fn the_local_repository_can_never_match_a_webhook_host() {
        let r = local_repo(&example_dir());
        assert_eq!(r.host, LOCAL_HOST);
        assert_eq!(r.path, "local/eustress-engine");
        assert!(!r.matches(FoundryProvider::GitHub, "github.com", "local/eustress-engine"));
    }

    #[tokio::test]
    async fn head_is_the_content_hash_and_fetch_checks_it() {
        let src = LocalDirSource::new(example_dir(), &["github:example-founder".to_string()]).unwrap();
        let head = src.branch_head(LOCAL_BRANCH).await.unwrap();
        assert_eq!(head.len(), 64);
        assert!(src.fetch_files(&head).await.is_ok());
        let other = "a".repeat(64);
        assert!(matches!(src.fetch_files(&other).await, Err(SourceError::Ambiguous(_))));
        let ctx = src.change_context(&head, None, NaiveDate::from_ymd_opt(2026, 11, 20).unwrap()).await.unwrap();
        assert!(!ctx.via_pull_request && ctx.approvers.is_empty());
        assert_eq!(ctx.actor, FoundryIdentity::github("example-founder"));
    }

    #[test]
    fn a_missing_file_is_named() {
        let dir = std::env::temp_dir().join(format!("msync-local-missing-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join(".msync")).unwrap();
        let errs = read_dir_files(&dir).unwrap_err();
        assert_eq!(errs.len(), 5);
        assert!(errs[0].contains("org.toml"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
