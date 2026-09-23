//! The IPC client lives in `ea_core::ipc` so `ea-daemon`'s tests can import
//! it too (a binary crate like `ea-cli` can't be depended on as a library).
pub use ea_core::ipc::Client;
