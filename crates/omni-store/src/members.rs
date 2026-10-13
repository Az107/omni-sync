//! Folder membership: which device holds which folder, and at which root.
//!
//! The table predates this module and was unused; adding a member is what lets
//! the dashboard show a folder's devices and roots. Membership is *recorded*,
//! not enforced — any enrolled device may still read or write any folder.

use rusqlite::{Connection, params};

use crate::StoreError;

/// One device's membership in a folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub device_id: String,
    /// The device's label, joined for display.
    pub device_name: String,
    /// The client-side root path, as raw bytes.
    pub root_path: Vec<u8>,
    /// `both`, `send` or `recv`.
    pub mode: String,
    /// The last sequence this member acknowledged.
    pub acked_seq: i64,
}

/// Create or update a membership. A device re-adding a folder with a new root
/// simply moves its root.
pub fn upsert(
    conn: &Connection,
    folder_id: &str,
    device_id: &str,
    root_path: &[u8],
    mode: &str,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO members (folder_id, device_id, root_path, mode)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (folder_id, device_id)
         DO UPDATE SET root_path = excluded.root_path, mode = excluded.mode",
        params![folder_id, device_id, root_path, mode],
    )?;
    Ok(())
}

/// Record how far a member has acknowledged the log. Only ever moves forward,
/// so a delayed or repeated pull cannot rewind it.
pub fn set_acked(
    conn: &Connection,
    folder_id: &str,
    device_id: &str,
    seq: i64,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE members
            SET acked_seq = ?3
          WHERE folder_id = ?1 AND device_id = ?2 AND acked_seq < ?3",
        params![folder_id, device_id, seq],
    )?;
    Ok(())
}

/// Every member of a folder, oldest device first, with its device name.
pub fn list(conn: &Connection, folder_id: &str) -> Result<Vec<Member>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT m.device_id, d.name, m.root_path, m.mode, m.acked_seq
           FROM members m
           JOIN devices d ON d.id = m.device_id
          WHERE m.folder_id = ?1
          ORDER BY d.enrolled_at",
    )?;
    let rows = stmt.query_map([folder_id], |row| {
        Ok(Member {
            device_id: row.get(0)?,
            device_name: row.get(1)?,
            root_path: row.get(2)?,
            mode: row.get(3)?,
            acked_seq: row.get(4)?,
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
    use crate::{devices, folders};

    fn seed(store: &crate::Store) {
        let folder = folders::Folder {
            id: "f1".into(),
            name: "Docs".into(),
            retention_versions: 50,
            retention_days: 90,
            encrypted: false,
        };
        store.with(|conn| folders::create(conn, &folder)).unwrap();
        let device = devices::Device {
            id: "d1".into(),
            name: "laptop".into(),
            pubkey: vec![],
            token_hash: vec![1u8; 32],
            revoked: false,
            last_seen_at: None,
        };
        store.with(|conn| devices::insert(conn, &device)).unwrap();
    }

    #[test]
    fn a_member_is_upserted_and_listed_with_its_device_name() {
        let store = crate::Store::open_in_memory().unwrap();
        seed(&store);

        store
            .with(|conn| upsert(conn, "f1", "d1", b"/home/me/Docs", "both"))
            .unwrap();
        let members = store.with(|conn| list(conn, "f1")).unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].device_name, "laptop");
        assert_eq!(members[0].root_path, b"/home/me/Docs");
        assert_eq!(members[0].mode, "both");

        // Re-adding moves the root rather than duplicating the row.
        store
            .with(|conn| upsert(conn, "f1", "d1", b"/srv/Docs", "send"))
            .unwrap();
        let members = store.with(|conn| list(conn, "f1")).unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].root_path, b"/srv/Docs");
        assert_eq!(members[0].mode, "send");
    }

    #[test]
    fn a_watermark_only_moves_forward() {
        let store = crate::Store::open_in_memory().unwrap();
        seed(&store);
        store
            .with(|conn| upsert(conn, "f1", "d1", b"/docs", "both"))
            .unwrap();

        store.with(|conn| set_acked(conn, "f1", "d1", 7)).unwrap();
        assert_eq!(store.with(|conn| list(conn, "f1")).unwrap()[0].acked_seq, 7);

        // A stale or repeated pull must not rewind the watermark.
        store.with(|conn| set_acked(conn, "f1", "d1", 3)).unwrap();
        assert_eq!(store.with(|conn| list(conn, "f1")).unwrap()[0].acked_seq, 7);

        // Only advances.
        store.with(|conn| set_acked(conn, "f1", "d1", 9)).unwrap();
        assert_eq!(store.with(|conn| list(conn, "f1")).unwrap()[0].acked_seq, 9);
    }
}
