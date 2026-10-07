//! SQLite storage: connection setup, migrations and repositories.
//!
//! Money/quantities are stored as decimal TEXT; financial aggregation happens
//! in Rust Decimal (no SQLite REAL sums). Foreign keys are always on.

pub mod ai_store;
pub mod app;
pub mod migrate;
pub mod store;
pub mod workbench;

use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;

/// Open a library database with the standard pragmas.
pub fn open(path: &Path) -> Result<Connection, rusqlite::Error> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    Ok(conn)
}

/// Open an in-memory database (tests).
pub fn open_in_memory() -> Result<Connection, rusqlite::Error> {
    let conn = Connection::open_in_memory()?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

/// Mutex-guarded single writer connection (system-architecture §4).
pub struct Writer {
    conn: Mutex<Connection>,
}

impl Writer {
    pub fn new(conn: Connection) -> Self {
        Self {
            conn: Mutex::new(conn),
        }
    }
    pub fn with<R>(&self, f: impl FnOnce(&Connection) -> R) -> R {
        let conn = self.conn.lock().expect("writer lock");
        f(&conn)
    }
}
