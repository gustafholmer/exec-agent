//! Deciding whether a scored event is worth interrupting a human for.
//!
//! Triage produces a number; this module turns that number into one of three
//! outcomes -- interrupt now, hold for the digest, or say nothing. The loop
//! that calls this (the scheduler) is a separate concern and lives elsewhere.
//!
//! [`telegram`] is the delivery end: the message the human sees, the two
//! buttons under it, and what happens when one of them is pressed. It is split
//! into decision logic ([`telegram::Notifier`]) and the wire
//! ([`telegram::TelegramTransport`]) so that everything worth getting wrong is
//! testable without a network.

pub mod log;
pub mod policy;
pub mod telegram;
pub mod updates;

pub use log::NotificationLog;
pub use policy::{NotificationPolicy, NotifyConfig, Verdict};
pub use telegram::{Notifier, TelegramConfig, TelegramTransport, TelegramUserId, Transport};
pub use updates::{OffsetStore, UpdateLoop, UpdateSource};
