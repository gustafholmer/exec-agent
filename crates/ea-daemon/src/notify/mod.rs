//! Deciding whether a scored event is worth interrupting a human for.
//!
//! Triage produces a number; this module turns that number into one of three
//! outcomes -- interrupt now, hold for the digest, or say nothing. The delivery
//! of the interruption (Telegram) and the loop that calls this (the scheduler)
//! are separate concerns and live elsewhere.

pub mod policy;

pub use policy::{NotificationPolicy, NotifyConfig, Verdict};
