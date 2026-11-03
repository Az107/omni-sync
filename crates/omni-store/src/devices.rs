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

/// Change a device's display label. Returns false when no such device exists.
pub fn rename(conn: &Connection, id: &str, name: &str) -> Result<bool, StoreError> {
    let changed = conn.execute(
        "UPDATE devices SET name = ?2 WHERE id = ?1",
        params![id, name],
    )?;
    Ok(changed > 0)
}

/// Mark a device revoked, which is what makes its token stop authenticating.
/// Idempotent: revoking an already-revoked device changes no timestamp. Returns
/// false when no such device exists.
pub fn revoke(conn: &Connection, id: &str) -> Result<bool, StoreError> {
    let changed = conn.execute(
        "UPDATE devices
            SET revoked_at = COALESCE(revoked_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
          WHERE id = ?1",
        [id],
    )?;
    Ok(changed > 0)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    fn seed(store: &Store) {
        let device = Device {
            id: "d1".into(),
            name: "laptop".into(),
            pubkey: vec![],
            token_hash: vec![1u8; 32],
            revoked: false,
            last_seen_at: None,
        };
        store.with(|conn| insert(conn, &device)).unwrap();
    }

    #[test]
    fn a_device_renames_itself() {
        let store = Store::open_in_memory().unwrap();
        seed(&store);

        assert!(
            store
                .with(|conn| rename(conn, "d1", "workstation"))
                .unwrap()
        );
        let device = store.with(|conn| get(conn, "d1")).unwrap().unwrap();
        assert_eq!(device.name, "workstation");

        // An unknown device is reported rather than silently accepted.
        assert!(!store.with(|conn| rename(conn, "nope", "x")).unwrap());
    }

    #[test]
    fn a_device_revokes_itself_and_its_token_stops_matching() {
        let store = Store::open_in_memory().unwrap();
        seed(&store);
        assert!(
            store
                .with(|conn| by_token_hash(conn, &[1u8; 32]))
                .unwrap()
                .is_some()
        );

        assert!(store.with(|conn| revoke(conn, "d1")).unwrap());
        let device = store
            .with(|conn| by_token_hash(conn, &[1u8; 32]))
            .unwrap()
            .unwrap();
        assert!(device.revoked);
        assert_eq!(store.with(count).unwrap(), 0);

        // Revoking twice stays revoked and keeps the original timestamp.
        let first = device_revoked_at(&store);
        assert!(store.with(|conn| revoke(conn, "d1")).unwrap());
        assert_eq!(device_revoked_at(&store), first);

        assert!(!store.with(|conn| revoke(conn, "nope")).unwrap());
    }

    fn device_revoked_at(store: &Store) -> Option<String> {
        store
            .with(|conn| {
                Ok(
                    conn.query_row("SELECT revoked_at FROM devices WHERE id = 'd1'", [], |r| {
                        r.get::<_, Option<String>>(0)
                    })?,
                )
            })
            .unwrap()
    }
}
