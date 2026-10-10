//! Forward-only migrations.
//!
//! Append a new SQL file to [`MIGRATIONS`]; never edit one that has been
//! applied. Each entry runs in its own transaction and records its version, so
//! a crash mid-upgrade leaves the database on the last complete version.

use rusqlite::Connection;

use crate::StoreError;

/// One SQL script per schema version, in order. Index + 1 is the version.
pub const MIGRATIONS: &[&str] = &[include_str!("../migrations/0001_baseline.sql")];

/// Apply every migration newer than the recorded schema version.
pub fn apply(conn: &mut Connection) -> Result<(), StoreError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             version    INTEGER PRIMARY KEY,
             applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
         );",
    )?;

    let applied: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;

    for (index, sql) in MIGRATIONS.iter().enumerate() {
        let version = (index + 1) as i64;
        if version <= applied {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO schema_migrations (version) VALUES (?1)",
            [version],
        )?;
        tx.commit()?;
        tracing::info!(version, "applied migration");
    }

    Ok(())
}
