use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tempfile::TempDir;

pub fn temp_store() -> (TempDir, Arc<Mutex<Connection>>) {
    let dir = TempDir::new().unwrap();
    let conn = crate::db::open(&dir.path().join("state.db")).unwrap();
    (dir, Arc::new(Mutex::new(conn)))
}
