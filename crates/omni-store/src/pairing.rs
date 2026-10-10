//! One-time pairing codes, consumed exactly once and only before expiry.

use rusqlite::{Connection, params};

use crate::StoreError;

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ', 'now')";

/// Create a code that expires `ttl_secs` from now.
pub fn create(conn: &Connection, code: &str, ttl_secs: i64) -> Result<(), StoreError> {
    // The modifier is built here, never interpolated from user input.
    let modifier = format!("+{ttl_secs} seconds");
    conn.execute(
        "INSERT INTO pairing_codes (code, expires_at)
         VALUES (?1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?2))",
        params![code, modifier],
    )?;
    Ok(())
}

/// Consume a code for a device. Returns `false` when the code is unknown,
/// already used, or expired — the caller must not distinguish these to a client.
pub fn consume(conn: &mut Connection, code: &str, device_id: &str) -> Result<bool, StoreError> {
    let tx = conn.transaction()?;
    let changed = tx.execute(
        &format!(
            "UPDATE pairing_codes
                SET consumed_at = {NOW}, consumed_device_id = ?2
              WHERE code = ?1
                AND consumed_at IS NULL
                AND expires_at > {NOW}"
        ),
        params![code, device_id],
    )?;
    tx.commit()?;
    Ok(changed == 1)
}

/// Housekeeping: drop expired, unused codes.
pub fn prune(conn: &Connection) -> Result<usize, StoreError> {
    let removed = conn.execute(
        &format!("DELETE FROM pairing_codes WHERE consumed_at IS NULL AND expires_at <= {NOW}"),
        [],
    )?;
    Ok(removed)
}
