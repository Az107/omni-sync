//! Device rows: enrollment, lookup by token, revocation.

use rusqlite::{Connection, OptionalExtension, params};

use crate::StoreError;

/// A registered device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// The device's Ed25519 public key (empty until the key exchange lands).
    pub pubkey: Vec<u8>,
    /// SHA-256 of the bearer token. The token itself is never stored.
    pub token_hash: Vec<u8>,
    pub revoked: bool,
    pub last_seen_at: Option<String>,
}

fn row_to_device(row: &rusqlite::Row<'_>) -> rusqlite::Result<Device> {
    Ok(Device {
        id: row.get(0)?,
        name: row.get(1)?,
        pubkey: row.get(2)?,
        token_hash: row.get(3)?,
        revoked: row.get(4)?,
        last_seen_at: row.get(5)?,
    })
}

pub fn insert(conn: &Connection, device: &Device) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO devices (id, name, pubkey, token_hash) VALUES (?1, ?2, ?3, ?4)",
        params![device.id, device.name, device.pubkey, device.token_hash],
    )?;
    Ok(())
}

pub fn by_token_hash(conn: &Connection, token_hash: &[u8]) -> Result<Option<Device>, StoreError> {
    conn.query_row(
        "SELECT id, name, pubkey, token_hash, revoked_at IS NOT NULL, last_seen_at
           FROM devices
          WHERE token_hash = ?1",
        [token_hash],
        row_to_device,
    )
    .optional()
    .map_err(StoreError::from)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Device>, StoreError> {
    conn.query_row(
        "SELECT id, name, pubkey, token_hash, revoked_at IS NOT NULL, last_seen_at
           FROM devices WHERE id = ?1",
        [id],
        row_to_device,
    )
    .optional()
    .map_err(StoreError::from)
}

pub fn list(conn: &Connection) -> Result<Vec<Device>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, name, pubkey, token_hash, revoked_at IS NOT NULL, last_seen_at
           FROM devices ORDER BY enrolled_at",
    )?;
    let rows = stmt.query_map([], row_to_device)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn count(conn: &Connection) -> Result<i64, StoreError> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM devices WHERE revoked_at IS NULL",
        [],
        |row| row.get(0),
    )?)
}

pub fn touch(conn: &Connection, id: &str) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE devices
            SET last_seen_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
          WHERE id = ?1",
        [id],
    )?;
    Ok(())
}

/// A device plus how long ago it was last seen, computed in SQLite so no date
/// parsing is needed on the Rust side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceSeen {
    pub id: String,
    pub name: String,
    pub revoked: bool,
    /// Seconds since the last contact; `None` means the device has never been seen.
    pub seen_seconds: Option<i64>,
}

pub fn list_with_seen(conn: &Connection) -> Result<Vec<DeviceSeen>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, name, revoked_at IS NOT NULL,
                CASE WHEN last_seen_at IS NULL THEN NULL
                     ELSE CAST(strftime('%s', 'now') AS INTEGER)
                          - CAST(strftime('%s', last_seen_at) AS INTEGER)
                END
           FROM devices ORDER BY enrolled_at",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(DeviceSeen {
            id: row.get(0)?,
            name: row.get(1)?,
            revoked: row.get(2)?,
            seen_seconds: row.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}
