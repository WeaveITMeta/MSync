//! The rank seam. Owners from engine configuration (`MSYNC_OWNER_MEMBER_IDS`) have rank
//! 1000. With no active CRA snapshot everyone else has no rank ([`EnvDirectory`]). With
//! one, ranks, roles and payee identities come from the snapshot ([`CraDirectory`]).

use std::collections::HashSet;

use chrono::NaiveDate;

use crate::cra::loader::CraHandle;
use crate::cra::{standing, FoundryIdentity, FoundryProvider};

/// Owners are engine configuration, never CRA content (contract section 2).
pub const OWNER_RANK: i64 = 1000;

/// Where member ranks (and, later, roles and payee identities) come from.
///
/// Fail closed: `None` means "no rank". A member with no rank never satisfies a
/// `min_approver_rank > 0`, an `eligibility.min_rank > 0`, or a strict-inequality check.
pub trait Directory: Send + Sync {
    /// Rank of a member, 0..=1000, or None when the member is unknown.
    fn rank_of(&self, member_id: &str) -> Option<i64>;

    /// The member a payee key (ledger `github_username`) belongs to, when the directory knows it.
    /// Round 1 knows none; payee identity comes from token `payee_keys` only.
    fn member_for_payee(&self, _payee_key: &str) -> Option<String> {
        None
    }

    /// Role keys a member holds. Round 1 knows none (eligibility with `roles` fails closed).
    fn roles_of(&self, _member_id: &str) -> Vec<String> {
        Vec::new()
    }
}

/// An owner entry in the provider form `github:<login>` or `gitlab:<login>`.
fn owner_foundry(entry: &str) -> Option<FoundryIdentity> {
    let (p, login) = entry.split_once(':')?;
    let login = login.trim();
    if login.is_empty() || login.contains(':') || login.contains(char::is_whitespace) {
        return None;
    }
    match p.trim().to_ascii_lowercase().as_str() {
        "github" => Some(FoundryIdentity::github(login)),
        "gitlab" => Some(FoundryIdentity::gitlab(login)),
        _ => None,
    }
}

/// Round-1 directory: owners listed in `MSYNC_OWNER_MEMBER_IDS` (comma separated) have rank 1000.
///
/// An entry is a member id (`mckale`) or a GitHub or GitLab identity (`github:Simbuilder`,
/// `gitlab:someone`). Only the provider form can bootstrap the first CRA snapshot. An entry
/// with a `:` that is not a valid GitHub or GitLab identity is ignored (it confers nothing).
#[derive(Debug, Clone, Default)]
pub struct EnvDirectory {
    owners: HashSet<String>,
    foundry_owners: Vec<FoundryIdentity>,
}

impl EnvDirectory {
    pub fn from_env() -> Self {
        Self::from_owner_list(&crate::config::env_var("MSYNC_OWNER_MEMBER_IDS").unwrap_or_default())
    }

    pub fn from_owner_list(list: &str) -> Self {
        Self::new(list.split(',').map(|s| s.trim().to_string()))
    }

    pub fn new<I: IntoIterator<Item = String>>(owners: I) -> Self {
        let mut ids = HashSet::new();
        let mut foundry = Vec::new();
        for o in owners.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            if o.contains(':') {
                match owner_foundry(&o) {
                    Some(f) => {
                        ids.insert(o);
                        foundry.push(f);
                    }
                    None => tracing::warn!("MSYNC_OWNER_MEMBER_IDS: ignoring malformed entry '{}'", o),
                }
            } else {
                ids.insert(o);
            }
        }
        Self { owners: ids, foundry_owners: foundry }
    }

    pub fn owner_count(&self) -> usize {
        self.owners.len()
    }

    /// Every owner entry, for [`crate::cra::authorize`].
    pub fn owner_entries(&self) -> Vec<String> {
        let mut v: Vec<String> = self.owners.iter().cloned().collect();
        v.sort();
        v
    }

    /// Is this GitHub or GitLab identity an owner (case-insensitive login)?
    pub fn is_owner_foundry(&self, provider: FoundryProvider, login: &str) -> bool {
        self.foundry_owners.iter().any(|o| o.matches(provider, login))
    }

    fn is_owner_id(&self, member_id: &str) -> bool {
        if self.owners.contains(member_id) {
            return true;
        }
        // A token bound to a provider-form owner id (`github:Simbuilder`) matches case-insensitively.
        owner_foundry(member_id).map_or(false, |f| self.is_owner_foundry(f.provider, &f.login))
    }
}

impl Directory for EnvDirectory {
    fn rank_of(&self, member_id: &str) -> Option<i64> {
        if self.is_owner_id(member_id) {
            Some(OWNER_RANK)
        } else {
            None
        }
    }
}

/// Ranks, roles and payee identities from the active CRA snapshot. With no snapshot active
/// every answer is exactly [`EnvDirectory`]'s.
///
/// - `rank_of`: owners (engine configuration) stay 1000, whether named by member id or by
///   one of the member's GitHub and GitLab identities. Any other CRA member gets their
///   [`crate::cra::standing`] rank, computed with their vested merit (today all merit counts
///   as vested). An id the snapshot does not list has no rank.
/// - `member_for_payee`: the member whose attested GitHub or GitLab login is the payee key.
/// - `roles_of`: active position keys plus the member's tier key.
pub struct CraDirectory {
    env: EnvDirectory,
    cra: CraHandle,
    today: Option<NaiveDate>,
}

impl CraDirectory {
    pub fn new(env: EnvDirectory, cra: CraHandle) -> Self {
        CraDirectory { env, cra, today: None }
    }

    /// Fix the evaluation date (tests). Default: today in UTC.
    pub fn with_today(mut self, today: NaiveDate) -> Self {
        self.today = Some(today);
        self
    }

    pub fn env(&self) -> &EnvDirectory {
        &self.env
    }

    pub fn handle(&self) -> &CraHandle {
        &self.cra
    }

    fn today(&self) -> NaiveDate {
        self.today.unwrap_or_else(|| chrono::Utc::now().date_naive())
    }

    fn member_is_owner(&self, snap: &crate::cra::Snapshot, member_id: &str) -> bool {
        let Some(m) = snap.member(member_id) else { return false };
        [(FoundryProvider::Github, &m.identities.github), (FoundryProvider::Gitlab, &m.identities.gitlab)]
            .iter()
            .any(|(p, l)| l.as_deref().map_or(false, |l| self.env.is_owner_foundry(*p, l)))
    }
}

/// `(provider, login)` of a ledger payee key: a bare GitHub login, `gitlab:<login>`,
/// `gitlab:<host>:<login>` or `github:<host>:<login>` (`foundry::legacy_username`).
fn payee_identity(key: &str) -> Option<(FoundryProvider, &str)> {
    let parts: Vec<&str> = key.split(':').collect();
    match parts.as_slice() {
        [login] => Some((FoundryProvider::Github, *login)),
        [p, login] | [p, _, login] => match p.to_ascii_lowercase().as_str() {
            "github" => Some((FoundryProvider::Github, *login)),
            "gitlab" => Some((FoundryProvider::Gitlab, *login)),
            _ => None,
        },
        _ => None,
    }
    .filter(|(_, l)| !l.is_empty())
}

impl Directory for CraDirectory {
    fn rank_of(&self, member_id: &str) -> Option<i64> {
        if let Some(r) = self.env.rank_of(member_id) {
            return Some(r);
        }
        let active = self.cra.active()?;
        let snap = &active.snapshot;
        if self.member_is_owner(snap, member_id) {
            return Some(OWNER_RANK);
        }
        standing(snap, member_id, self.cra.merit_of(member_id), self.today()).map(|s| i64::from(s.rank))
    }

    fn member_for_payee(&self, payee_key: &str) -> Option<String> {
        let active = self.cra.active()?;
        let (provider, login) = payee_identity(payee_key.trim())?;
        if let Some(m) = active.snapshot.member_by_foundry(&FoundryIdentity { provider, login: login.to_string() }) {
            return Some(m.id.clone());
        }
        // An owner configured only by GitHub or GitLab identity is the payee of their own login.
        if self.env.is_owner_foundry(provider, login) {
            return Some(format!("{}:{}", provider.as_str(), login));
        }
        None
    }

    fn roles_of(&self, member_id: &str) -> Vec<String> {
        let Some(active) = self.cra.active() else { return Vec::new() };
        let Some(st) = standing(&active.snapshot, member_id, self.cra.merit_of(member_id), self.today()) else {
            return Vec::new();
        };
        if st.status != crate::cra::MemberStatus::Active {
            return Vec::new();
        }
        let mut roles: Vec<String> = st.positions.iter().map(|p| p.key.clone()).collect();
        if let Some(t) = st.tier {
            roles.push(t.key);
        }
        roles
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_list_accepts_member_ids_and_foundry_identities() {
        let d = EnvDirectory::from_owner_list(" mckale , github:Simbuilder, gitlab:someone, bitbucket:x, github:, github:a:b ");
        assert_eq!(d.owner_entries(), vec!["github:Simbuilder", "gitlab:someone", "mckale"]);
        assert_eq!(d.rank_of("mckale"), Some(OWNER_RANK));
        assert_eq!(d.rank_of("github:simbuilder"), Some(OWNER_RANK));
        assert!(d.is_owner_foundry(FoundryProvider::Github, "SIMBUILDER"));
        assert!(!d.is_owner_foundry(FoundryProvider::Gitlab, "Simbuilder"));
        assert_eq!(d.rank_of("bitbucket:x"), None, "a malformed entry confers nothing");
        assert_eq!(d.rank_of("someone"), None);
    }

    #[test]
    fn payee_keys_map_to_foundry_identities() {
        assert_eq!(payee_identity("alice"), Some((FoundryProvider::Github, "alice")));
        assert_eq!(payee_identity("gitlab:alice"), Some((FoundryProvider::Gitlab, "alice")));
        assert_eq!(payee_identity("gitlab:git.example.org:alice"), Some((FoundryProvider::Gitlab, "alice")));
        assert_eq!(payee_identity("github:ghe.example.org:alice"), Some((FoundryProvider::Github, "alice")));
        assert_eq!(payee_identity("svn:alice"), None);
        assert_eq!(payee_identity("gitlab:"), None);
    }
}
