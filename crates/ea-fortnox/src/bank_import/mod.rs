//! Bank import: an `.xlsx` bank export in, approved vouchers out.
//!
//! Upstream this is `packages/fortnox/src/bank-import/`, seven modules and five
//! test files. The Rust mirrors it module for module, with `match.ts` renamed
//! [`match_rules`] because `match` is a keyword.
//!
//! # The pipeline
//!
//! ```text
//!   workbook::read_seb      ─┐
//!   workbook::read_kvitton  ─┴→ proposal::build_proposal ─→ Förslag rows
//!                                    │  match_rules::match_all
//!                                    └  code::code_line
//!
//!   workbook::read_forslag  ──→ workbook::merge_forslag  ─→ the user's edits win
//!                                    ↓
//!                             workbook::write_workbook
//!                                    ↓
//!                              [ a human writes J ]
//!                                    ↓
//!   workbook::read_forslag  ──→ post::run_post ──→ voucher::forslag_to_payload
//! ```
//!
//! # None of this is wired up yet
//!
//! The pipeline above is complete and tested, and **nothing outside these
//! modules' own tests calls any of it**. No MCP tool, binary, daemon job or
//! CLI command reaches [`post::run_post`]. Before that changes, read the
//! warning on [`post::run_post`] itself: it carries its own posting client,
//! so a caller that is not routed through `propose_action` bypasses the
//! approval gate that every other write in this system goes through.
//!
//! Two properties hold the whole thing together, and each has a module that
//! exists for it:
//!
//! * **Nothing is posted that a human has not approved.** The proposer never
//!   writes `J`, and [`post::run_post`] posts nothing without both a `J` in the
//!   sheet and `commit: true` on the call.
//! * **Nothing is posted twice.** Every voucher carries `[imp:<Rad_id>]`, and
//!   `Rad_id` is [`voucher::rad_id`] — pinned byte-for-byte against the
//!   TypeScript, because the vouchers already in Fortnox carry keys the
//!   TypeScript computed.
//!
//! # Where the Rust deliberately disagrees with the TypeScript
//!
//! Each is argued where it lives; this is the index.
//!
//! | module | divergence |
//! |--------|------------|
//! | [`workbook`] | writing rebuilds the file rather than editing one sheet of it — there is no read-modify-write path from `calamine` to `rust_xlsxwriter` |
//! | [`workbook`] | a malformed date or amount errors with its row number instead of vanishing |
//! | [`code`] | `LON` is listed last, because upstream's ordering makes its own `HALLON` entry unreachable |
//! | [`code`] | an unsupported or fractional `Momssats` errors instead of producing a plausible-looking split |
//! | [`voucher`] | no `VoucherMappingError` type: upstream's only caller does not distinguish it |
//! | [`post`] | the financial year is overridable rather than hardcoded to one that has now ended |
//!
//! # `proposal` is not a translation
//!
//! [`proposal`] has no upstream test. Its tests were written here rather than
//! ported, and it is held to this project's standard rather than judged as a
//! translation. Its module docs say so.

pub mod code;
pub mod match_rules;
pub mod post;
pub mod proposal;
pub mod types;
pub mod voucher;
pub mod workbook;

pub use proposal::build_proposal;
pub use types::{Confidence, Direction, ForslagRow, KvittoRow, SebRow};
