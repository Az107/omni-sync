//! The client-side store: the last-synced manifest and the applied watermark.

use omni_core::{ChunkRef, FileMeta, Manifest, PathBytes};
use rusqlite::{Connection, OptionalExtension, params};

use crate::StoreError;

/// The last-synced manifest for a folder: what this device believes is
/// committed on the server.
pub fn manifest(conn: &Connection, folder_id: &str) -> Result<Manifest, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT path, size, mtime_ns, mode, chunks_json
           FROM file_state WHERE folder_id = ?1",
    )?;
    let raw = stmt
        .query_map([folder_id], |row| {
            Ok((
                row.get::<_, PathBytes>(0)?,
                row.get::<_, i64>(1)? as u64,
                row.get::<_, i64>(2)?,
                row.get::<_, u32>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut out = Manifest::new();
    for (path, size, mtime_ns, mode, chunks_json) in raw {
        let chunks: Vec<ChunkRef> =
            serde_json::from_str(&chunks_json).map_err(|e| StoreError::Json(e.to_string()))?;
        out.insert(
            path,
            FileMeta {
                size,
                mtime_ns,
                mode,
                chunks,
            },
        );
    }
    Ok(out)
}

/// Record that a path is now synced with the given content and version.
pub fn set_file(
    conn: &Connection,
    folder_id: &str,
    path: &[u8],
    meta: &FileMeta,
    version_id: i64,
) -> Result<(), StoreError> {
    let chunks_json =
        serde_json::to_string(&meta.chunks).map_err(|e| StoreError::Json(e.to_string()))?;
    conn.execute(
        "INSERT INTO file_state (folder_id, path, size, mtime_ns, mode, chunks_json, version_id, held_reason)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)
         ON CONFLICT (folder_id, path) DO UPDATE
            SET size = excluded.size, mtime_ns = excluded.mtime_ns,
                mode = excluded.mode, chunks_json = excluded.chunks_json,
                version_id = excluded.version_id, held_reason = NULL",
        params![
            folder_id,
            path,
            meta.size as i64,
            meta.mtime_ns,
            meta.mode,
            chunks_json,
            version_id
        ],
    )?;
    conn.execute(
        "DELETE FROM held_files WHERE folder_id = ?1 AND path = ?2",
        params![folder_id, path],
    )?;
    Ok(())
}

/// Map each known path to the server version the client last synced.
pub fn versions(
    conn: &Connection,
    folder_id: &str,
) -> Result<std::collections::BTreeMap<PathBytes, i64>, StoreError> {
    let mut stmt = conn.prepare("SELECT path, version_id FROM file_state WHERE folder_id = ?1")?;
    let rows = stmt.query_map([folder_id], |row| {
        Ok((row.get::<_, PathBytes>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut out = std::collections::BTreeMap::new();
    for row in rows {
        let (path, version_id) = row?;
        out.insert(path, version_id);
    }
    Ok(out)
}

/// Forget a path (after a delete has been synced).
pub fn remove_file(conn: &Connection, folder_id: &str, path: &[u8]) -> Result<(), StoreError> {
    conn.execute(
        "DELETE FROM file_state WHERE folder_id = ?1 AND path = ?2",
        params![folder_id, path],
    )?;
    conn.execute(
        "DELETE FROM held_files WHERE folder_id = ?1 AND path = ?2",
        params![folder_id, path],
    )?;
    Ok(())
}

/// Record or clear why a path is held (`held_by_pid:1234`, `process:retroarch`,
/// `quiet_until`).
pub fn set_held(
    conn: &Connection,
    folder_id: &str,
    path: &[u8],
    reason: Option<&str>,
) -> Result<(), StoreError> {
    match reason {
        Some(reason) => {
            conn.execute(
                "INSERT INTO held_files (folder_id, path, held_reason)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT (folder_id, path) DO UPDATE
                    SET held_reason = excluded.held_reason",
                params![folder_id, path, reason],
            )?;
            conn.execute(
                "UPDATE file_state SET held_reason = ?3
                  WHERE folder_id = ?1 AND path = ?2",
                params![folder_id, path, reason],
            )?;
        }
        None => {
            conn.execute(
                "DELETE FROM held_files WHERE folder_id = ?1 AND path = ?2",
                params![folder_id, path],
            )?;
            conn.execute(
                "UPDATE file_state SET held_reason = NULL
                  WHERE folder_id = ?1 AND path = ?2",
                params![folder_id, path],
            )?;
        }
    }
    Ok(())
}

/// Clear every hold recorded for a folder.
pub fn clear_held_folder(conn: &Connection, folder_id: &str) -> Result<(), StoreError> {
    conn.execute("DELETE FROM held_files WHERE folder_id = ?1", [folder_id])?;
    conn.execute(
        "UPDATE file_state SET held_reason = NULL WHERE folder_id = ?1",
        [folder_id],
    )?;
    Ok(())
}

/// Every currently held path in a folder, mapped to its `held_reason`.
pub fn held_files(
    conn: &Connection,
    folder_id: &str,
) -> Result<std::collections::BTreeMap<PathBytes, String>, StoreError> {
    let mut stmt = conn
        .prepare("SELECT path, held_reason FROM held_files WHERE folder_id = ?1 ORDER BY path")?;
    let rows = stmt.query_map([folder_id], |row| {
        Ok((row.get::<_, PathBytes>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = std::collections::BTreeMap::new();
    for row in rows {
        let (path, reason) = row?;
        out.insert(path, reason);
    }
    Ok(out)
}

/// The highest op id this device has applied for a folder.
pub fn applied_seq(conn: &Connection, folder_id: &str) -> Result<i64, StoreError> {
    let seq = conn
        .query_row(
            "SELECT applied_seq FROM sync_state WHERE folder_id = ?1",
            [folder_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(seq.unwrap_or(0))
}

pub fn set_applied_seq(conn: &Connection, folder_id: &str, seq: i64) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO sync_state (folder_id, applied_seq) VALUES (?1, ?2)
         ON CONFLICT (folder_id) DO UPDATE SET applied_seq = excluded.applied_seq",
        params![folder_id, seq],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    fn meta(chunks: Vec<ChunkRef>) -> FileMeta {
        FileMeta {
            size: chunks.iter().map(|c| c.len).sum(),
            mtime_ns: 5,
            mode: 0o644,
            chunks,
        }
    }

    #[test]
    fn file_state_round_trips_and_updates() {
        let store = Store::open_agent_in_memory().unwrap();
        let path = b"a.txt".as_slice();
        store
            .with(|conn| set_file(conn, "f1", path, &meta(vec![ChunkRef::new("aa", 3)]), 7))
            .unwrap();

        let synced = store.with(|conn| manifest(conn, "f1")).unwrap();
        assert_eq!(
            synced.get(path).unwrap().chunks,
            vec![ChunkRef::new("aa", 3)]
        );
        assert_eq!(
            store
                .with(|conn| versions(conn, "f1"))
                .unwrap()
                .get(path)
                .copied(),
            Some(7)
        );

        // Updating the same path replaces the row.
        store
            .with(|conn| set_file(conn, "f1", path, &meta(vec![ChunkRef::new("bb", 9)]), 12))
            .unwrap();
        let synced = store.with(|conn| manifest(conn, "f1")).unwrap();
        assert_eq!(synced.len(), 1);
        assert_eq!(
            synced.get(path).unwrap().chunks,
            vec![ChunkRef::new("bb", 9)]
        );

        store.with(|conn| remove_file(conn, "f1", path)).unwrap();
        assert!(store.with(|conn| manifest(conn, "f1")).unwrap().is_empty());
    }

    #[test]
    fn the_applied_watermark_starts_at_zero_and_advances() {
        let store = Store::open_agent_in_memory().unwrap();
        assert_eq!(store.with(|conn| applied_seq(conn, "f1")).unwrap(), 0);
        store.with(|conn| set_applied_seq(conn, "f1", 12)).unwrap();
        assert_eq!(store.with(|conn| applied_seq(conn, "f1")).unwrap(), 12);
        store.with(|conn| set_applied_seq(conn, "f1", 20)).unwrap();
        assert_eq!(store.with(|conn| applied_seq(conn, "f1")).unwrap(), 20);
    }

    #[test]
    fn held_paths_are_recorded_and_cleared_on_sync() {
        let store = Store::open_agent_in_memory().unwrap();
        let path = b"save.dat".as_slice();
        store
            .with(|conn| set_held(conn, "f1", path, Some("held_by_pid:4123")))
            .unwrap();
        let held = store.with(|conn| held_files(conn, "f1")).unwrap();
        assert_eq!(held.get(path).map(String::as_str), Some("held_by_pid:4123"));

        // Once the file syncs, its hold is cleared automatically.
        store
            .with(|conn| set_file(conn, "f1", path, &meta(vec![ChunkRef::new("aa", 3)]), 1))
            .unwrap();
        assert!(
            store
                .with(|conn| held_files(conn, "f1"))
                .unwrap()
                .is_empty()
        );
    }
}
