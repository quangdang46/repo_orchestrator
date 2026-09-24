//! Fake implementations for testing.
//!
//! Fake providers, clients, and other external dependencies.

use ro_state::open_db;
use std::path::Path;

/// Create an in-memory SQLite DB for testing.
pub fn memory_db() -> rusqlite::Connection {
    open_db(Path::new(":memory:")).expect("open memory db")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_db_opens() {
        let conn = memory_db();
        assert!(conn.is_autocommit());
    }
}
