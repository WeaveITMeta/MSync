use super::ledger::LedgerError;
use super::policy::PolicyError;
use crate::auth::AuthError;

/// Errors from the payout control functions (approvals, runs, blocks, policy).
#[derive(Debug, thiserror::Error)]
pub enum PayoutError {
    /// Missing scope (403) or, for library callers, any auth failure.
    #[error("{0}")]
    Auth(#[from] AuthError),
    #[error("{0} not found")]
    NotFound(&'static str),
    #[error("{0}")]
    InvalidState(String),
    /// A payee can never approve their own payout. Not configurable.
    #[error("a payee cannot approve their own payout")]
    PayeeCannotApprove,
    /// A two-person or payee rule refused the action (payroll, contract 11.5): the creator or
    /// last editor approving their own order, a payee creating, editing or approving what pays
    /// them, the submitter approving their own invoice. Not configurable.
    #[error("{0}")]
    Forbidden(String),
    /// policy.enabled = false: nothing is prepared or paid.
    #[error("payouts are disabled by policy")]
    PolicyDisabled,
    #[error("invalid: {}", .0.join("; "))]
    Invalid(Vec<String>),
    #[error("{0}")]
    Policy(#[from] PolicyError),
    #[error("{0}")]
    Ledger(#[from] LedgerError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}
