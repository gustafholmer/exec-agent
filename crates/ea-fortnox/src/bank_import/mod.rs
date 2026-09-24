//! Bank import: an `.xlsx` bank export in, reviewed and approved vouchers out.

pub mod types;
pub mod workbook;

pub use types::{Confidence, Direction, ForslagRow, KvittoRow, SebRow};
