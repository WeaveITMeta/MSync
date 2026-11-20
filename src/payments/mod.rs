//! Contribution payment model: tracked-branch config, payout ledger,
//! Stripe Connect client, signature helpers and HTTP routes.
//!
//! Invariant: money only moves in `ledger::claim_and_transfer` (reached through `ledger::approve`
//! and `ledger::pay_guarded`), behind the atomic status claim and the `dsync-ledger-<id>`
//! idempotency key. Callers: the admin-token approve route and payout runs.
//!
//! Payroll (contract 11): `payees`, `payorders`, `invoices` and `statement`. Pay orders and invoices
//! only write ledger rows (`ledger::insert_source_row`); payout runs pay them like merit rows.
//!
//! Hourly pay and time (contract 14): `wages`, `shifts`, `timeclock` and `timesheet`. A
//! timesheet only writes ledger rows (source `timesheet`), from the scheduler tick.

pub mod api;
pub mod approvals;
pub mod config;
pub mod error;
pub mod invoices;
pub mod ledger;
pub mod onboarding;
pub mod payees;
pub mod payorders;
pub mod payroll_api;
pub mod policy;
pub mod routes;
pub mod runs;
pub mod schedule;
pub mod shifts;
pub mod signature;
pub mod statement;
pub mod stripe;
pub mod time_api;
pub mod timeclock;
pub mod timesheet;
pub mod wages;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod controls_tests;
#[cfg(test)]
mod payroll_tests;
#[cfg(test)]
mod time_tests;

/// Test-only fault injection for hourly pay and time: `MSYNC_TIME_MUTANT=<name>` switches one
/// deliberate bug on, so each test can be shown to fail against the bug it guards. Compiled to
/// `false` outside test builds.
pub(crate) mod mutant {
    #[cfg(test)]
    pub fn on(name: &str) -> bool {
        std::env::var("MSYNC_TIME_MUTANT").map_or(false, |v| v == name)
    }

    #[cfg(not(test))]
    #[inline(always)]
    pub fn on(_name: &str) -> bool {
        false
    }
}

pub use config::PaymentConfig;
pub use error::PayoutError;
pub use policy::PayoutPolicy;
pub use stripe::{StripeApi, StripeClient, TransferRequest};
