//! The rank seam. Round 1: owners from engine configuration (`DSYNC_OWNER_MEMBER_IDS`)
//! have rank 1000 and everyone else has no rank. A later round plugs the CRA in here.

use std::collections::HashSet;

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

/// Round-1 directory: owners listed in `DSYNC_OWNER_MEMBER_IDS` (comma separated) have rank 1000.
#[derive(Debug, Clone, Default)]
pub struct EnvDirectory {
    owners: HashSet<String>,
}

impl EnvDirectory {
    pub fn from_env() -> Self {
        Self::from_owner_list(&std::env::var("DSYNC_OWNER_MEMBER_IDS").unwrap_or_default())
    }

    pub fn from_owner_list(list: &str) -> Self {
        Self::new(list.split(',').map(|s| s.trim().to_string()))
    }

    pub fn new<I: IntoIterator<Item = String>>(owners: I) -> Self {
        Self {
            owners: owners.into_iter().filter(|s| !s.is_empty()).collect(),
        }
    }

    pub fn owner_count(&self) -> usize {
        self.owners.len()
    }
}

impl Directory for EnvDirectory {
    fn rank_of(&self, member_id: &str) -> Option<i64> {
        if self.owners.contains(member_id) {
            Some(OWNER_RANK)
        } else {
            None
        }
    }
}
