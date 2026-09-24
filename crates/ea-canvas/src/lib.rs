//! `ea-canvas` — read-only Canvas LMS access, as a stdio MCP server.
//!
//! The first connector that talks to something real. It exists to answer one
//! question well: *what is due, and when.* Courses and assignments are read
//! from Canvas's REST API with an account access token; nothing in this crate
//! can write to Canvas.
//!
//! Two properties are load-bearing and are pinned by tests rather than by
//! good intentions:
//!
//! * **Failures are loud.** [`tools::CanvasServer::watch_poll`] returns an
//!   error when Canvas does, never an empty array. `[]` on failure is
//!   indistinguishable from "nothing is due", so the daemon's circuit breaker
//!   would never trip and an expired token would simply go quiet.
//! * **The write door is shut before it exists.** `connectors/canvas/policy.toml`
//!   declares `submit_assignment = "deny"` although no such tool is
//!   implemented. The gate matches by name, so the rule binds the moment
//!   anybody adds one — see [`tools::DELIBERATE_PLACEHOLDERS`].
//!
//! The access token is a bearer credential for the owner's entire Canvas
//! account; [`client`] documents how it is stored and why no error path in
//! this crate can print it.
#![forbid(unsafe_code)]

pub mod client;
pub mod tools;
