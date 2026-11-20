//! API tokens, principals, scopes and the rank seam (contract section 7).
//!
//! Tokens are `msync_<id>_<secret>`, stored as SHA-256 hex, bound to a member id. The actor on
//! every write is the token's member; no request body names its own actor.

pub mod directory;
pub mod routes;
pub mod tokens;

pub use directory::{CraDirectory, Directory, EnvDirectory, OWNER_RANK};
pub use tokens::{
    authenticate, authenticate_token, create_token, is_payee, member_payee_keys, revoke_token,
    AuthError, CreatedToken, NewToken, Principal, TokenCreator, TokenError, DEFAULT_ORG,
    LEGACY_ADMIN_MEMBER,
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
    // Payroll control (contract section 11.6).
    pub const PAYEES_READ: &str = "payees.read";
    pub const PAYEES_MANAGE: &str = "payees.manage";
    pub const PAYORDERS_READ: &str = "payorders.read";
    pub const PAYORDERS_MANAGE: &str = "payorders.manage";
    pub const PAYORDERS_APPROVE: &str = "payorders.approve";
    pub const INVOICES_SUBMIT: &str = "invoices.submit";
    pub const INVOICES_READ: &str = "invoices.read";
    pub const INVOICES_APPROVE: &str = "invoices.approve";
    // Audit log (contract 13.1).
    pub const AUDIT_READ: &str = "audit.read";
    // MSync as an OAuth provider (contract 12.3): register platforms, rotate the signing key.
    pub const OAUTH_CLIENTS: &str = "oauth.clients";
    // Hourly pay and time (contract 14.6). `time.clock` only ever acts on the token member's
    // own payee: no request names a payee.
    pub const TIME_CLOCK: &str = "time.clock";
    pub const TIME_READ: &str = "time.read";
    pub const TIME_MANAGE: &str = "time.manage";
    pub const TIME_APPROVE: &str = "time.approve";

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
        PAYEES_READ,
        PAYEES_MANAGE,
        PAYORDERS_READ,
        PAYORDERS_MANAGE,
        PAYORDERS_APPROVE,
        INVOICES_SUBMIT,
        INVOICES_READ,
        INVOICES_APPROVE,
        AUDIT_READ,
        OAUTH_CLIENTS,
        TIME_CLOCK,
        TIME_READ,
        TIME_MANAGE,
        TIME_APPROVE,
    ];
}

#[cfg(test)]
mod tests;
