//! Contribution payment model: tracked-branch config, payout ledger,
//! Stripe Connect client, signature helpers and HTTP routes.
//!
//! Invariant: money only moves in `ledger::approve`, which is only reachable
//! from the admin-token-protected approve endpoint.

pub mod config;
pub mod ledger;
pub mod onboarding;
pub mod routes;
pub mod signature;
pub mod stripe;

#[cfg(test)]
mod tests;

pub use config::PaymentConfig;
pub use stripe::{StripeApi, StripeClient, TransferRequest};