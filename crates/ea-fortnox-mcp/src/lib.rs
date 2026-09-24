//! `ea-fortnox-mcp` — the Fortnox connector's MCP tool surface.
//!
//! [`ea_fortnox`] is the library: money, the BAS chart, VAT, vouchers,
//! reporting, OAuth, the HTTP client. This crate is the thin layer that
//! exposes it to a language model as MCP tools, and it exists mainly to hold
//! one design decision.
//!
//! # One confirmation, not two
//!
//! The TypeScript this is ported from guards every write with a `confirm`
//! parameter: the tool renders a preview when called without it and posts when
//! called again with `confirm: true`. That is a sound design for an MCP server
//! a person drives from a chat window, and it is the wrong one here, because
//! this daemon already has a gate. A session calls `propose_action`,
//! deterministic Rust in `ea-core`'s [`Policy`] decides auto-execute /
//! queue-for-a-human / refuse, and a queued action waits for a tap in
//! Telegram before the executor ever calls the tool.
//!
//! Keeping `confirm` as well would mean two confirmation mechanisms in series,
//! and two means one of them is the one people stop reading. So:
//!
//! * **The write tools have no `confirm` parameter at all** and post when
//!   called. Reaching one means the gate already approved it. `tools::write`
//!   and `tools::attach` say so in every description, and
//!   `no_write_tool_takes_a_confirm_parameter` pins it against a future edit
//!   that reintroduces the flag.
//! * **The preview each write used to render is its own read-only tool** —
//!   [`tools::preview`] — which posts nothing and needs no credentials. The
//!   text is upstream's, because that string is what a human reads on their
//!   phone before approving a voucher, and it has been read by a real person
//!   approving real vouchers. Only its first line changed, and only because
//!   the sentence it carried was an instruction to re-run with `confirm:true`.
//!
//! [`Policy`]: ea_core::policy::Policy
#![forbid(unsafe_code)]

pub mod config;
pub mod tools;
