//! `ea-fortnox` — Swedish bookkeeping against the Fortnox API.
//!
//! This crate is a port of a TypeScript library that has been filing a real
//! Swedish AB's VAT returns through the live Fortnox API. The TypeScript is
//! the specification; every module here was translated test-first from its
//! `.test.ts` counterpart, and every place the Rust *deliberately* disagrees
//! with the TypeScript carries a comment saying so.
//!
//! The one systematic disagreement is arithmetic. The TypeScript computes
//! money in IEEE-754 doubles and patches the damage with a `round2` helper
//! that nudges by `Number.EPSILON` before rounding. This crate computes in
//! [`rust_decimal::Decimal`], which is exact for the base-10 quantities money
//! actually is, so the nudge has no analogue and no need of one. Where an
//! upstream expectation encoded a float artifact, the Rust expectation is the
//! exact value and says which TypeScript value it replaced.
//!
//! [`domain::money`] holds the rounding rule everything else depends on;
//! [`domain::bas`] holds the Swedish BAS chart of accounts.
//!
//! [`auth`] holds the OAuth flow and the rotating-refresh-token store, and
//! [`errors`] the one error type it returns.

pub mod auth;
pub mod domain;
pub mod errors;
pub mod reporting;

pub use domain::{bas, moms, money, posting, voucher};
pub use errors::FortnoxError;
