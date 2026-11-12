//! Contribution payment model: tracked-branch config, payout ledger,
//! Stripe Connect client, signature helpers and HTTP routes.
//!
//! Invariant: money only moves in `ledger::claim_and_transfer` (reached through `ledger::approve`
//! and `ledger::pay_guarded`), behind the atomic status claim and the `dsync-ledger-<id>`
//! idempotency key. Callers: the admin-token approve route and payout runs.

pub mod api;
pub mod approvals;
pub mod config;
pub mod error;
pub mod ledger;
pub mod onboarding;
pub mod policy;
pub mod routes;
pub mod runs;
pub mod schedule;
pub mod signature;
pub mod stripe;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod controls_tests;

pub use config::PaymentConfig;
pub use error::PayoutError;
pub use policy::PayoutPolicy;
pub use stripe::{StripeApi, StripeClient, TransferRequest};
