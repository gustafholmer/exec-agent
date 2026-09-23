//! The daemon, as a library.
//!
//! The `ea-daemon` binary is a thin shell over this crate: everything of
//! substance lives in these modules so that it can be tested, and used, from
//! outside the process. The executor needs [`connectors::Registry`] from
//! another crate, and the IPC surface needs integration tests that spawn a real
//! socket; neither is possible against a bin-only crate.
//!
//! Note what is deliberately *not* here: no path from the control socket to
//! [`connectors::Registry::call`]. A tool call is an action, and actions reach
//! a connector only through the policy gate in `ea_core::policy` by way of the
//! executor. Wiring a tool-invocation method into the IPC server would hand
//! every client of the socket arbitrary tool calls with the gate bypassed.

pub mod connectors;
pub mod daemon;
pub mod executor;
pub mod ipc;
pub mod notify;
pub mod session;
pub mod triage;
