//! API tokens, principals, scopes and the rank seam (contract section 7).
//!
//! Tokens are `dsync_<id>_<secret>`, stored as SHA-256 hex, bound to a member id. The actor on
//! every write is the token's member; no request body names its own actor.

pub mod directory;
pub mod routes;
pub mod tokens;

pub use directory::{Directory, EnvDirectory, OWNER_RANK};
pub use tokens::{
    authenticate, authenticate_token, create_token, is_payee, member_payee_keys, revoke_token, AuthError,
    CreatedToken, NewToken, Principal, TokenCreator, TokenError, DEFAULT_ORG, LEGACY_ADMIN_MEMBER,
};

/// Scopes (contract section 7).
pub mod scopes {
    pub const HIERARCHY_READ: &str = "hierarchy.read";
    pub const MERIT_READ: &str = "merit.read";
    pub const MERIT_SCORE: &str = "merit.score";
    pub const PAYOUTS_READ: &str = "payouts.read";
    pub const PAYOUTS_PREPARE: &str = "payouts.prepare";
    pub const PAYOUTS_APPROVE: &str = "payouts.approve";
    pub const PAYOUTS_EXECUTE: &str = "payouts.execute";
    pub const PAYOUTS_POLICY: &str = "payouts.policy";
    pub const TOKENS_MANAGE: &str = "tokens.manage";

    pub const ALL: &[&str] = &[
        HIERARCHY_READ,
        MERIT_READ,
        MERIT_SCORE,
        PAYOUTS_READ,
        PAYOUTS_PREPARE,
        PAYOUTS_APPROVE,
        PAYOUTS_EXECUTE,
        PAYOUTS_POLICY,
        TOKENS_MANAGE,
    ];
}

#[cfg(test)]
mod tests;
