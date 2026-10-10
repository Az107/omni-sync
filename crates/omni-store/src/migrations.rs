//! Forward-only migrations.
//!
//! Append a new SQL file to the relevant list; never edit one that has been
//! applied. Each entry runs in its own transaction and records its version, so
//! a crash mid-upgrade leaves the database on the last complete version.
//!
//! There are two schema families: the server (`MIGRATIONS`) and the client
//! (`AGENT_MIGRATIONS`). They live in separate database files and never mix.

use rusqlite::Connection;

use crate::StoreError;

/// Server schema versions, in order. Index + 1 is the version.
pub const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_baseline.sql"),
    include_str!("../migrations/0002_ops_base_version.sql"),
];

/// Client (agent) schema versions, in order.
pub const AGENT_MIGRATIONS: &[&str] = &[include_str!("../migrations/agent_0001.sql")];

/// Apply the server migrations that are newer than the recorded version.
pub fn apply(conn: &mut Connection) -> Result<(), StoreError> {
    apply_list(conn, MIGRATIONS)
}

/// Apply the agent migrations that are newer than the recorded version.
pub fn apply_agent(conn: &mut Connection) -> Result<(), StoreError> {
    apply_list(conn, AGENT_MIGRATIONS)
}

fn apply_list(conn: &mut Connection, list: &[&str]) -> Result<(), StoreError> {
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

    for (index, sql) in list.iter().enumerate() {
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
