//! Applying ops: the server's sequencer, in one transaction.
//!
//! Every op does four things together, or none of them:
//! 1. append to the op log (`ops`), which is what a polling client tails;
//! 2. create an immutable `versions` row (the content);
//! 3. point `files.active_version_id` at it (or mark the path deleted);
//! 4. if the sender's base was stale, insert an `overwrites` row for review.
//!
//! Chunk refcounts are bumped for anything the version references.

use omni_core::{ChunkRef, Decision, Op, OpKind, PathBytes, Sequencer};
use rusqlite::{Connection, OptionalExtension, params};

use crate::StoreError;

/// The result of applying one op.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppliedOp {
    /// The op id — the client's `applied_seq` watermark.
    pub seq: i64,
    /// The version row this op created.
    pub version_id: i64,
    /// Set when the sender overwrote a version it had not seen.
    pub overwrite: Option<NewOverwrite>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NewOverwrite {
    pub losing_version_id: i64,
    pub winning_version_id: i64,
}

/// A file's current state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveFile {
    pub path: PathBytes,
    pub version_id: i64,
    pub size: u64,
    pub mtime_ns: i64,
    pub mode: u32,
    pub chunks: Vec<ChunkRef>,
}

/// One entry of the op log, with the content it produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    pub seq: i64,
    pub version_id: i64,
    pub kind: String,
    pub path: PathBytes,
    pub device_id: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub mode: u32,
    pub chunks: Vec<ChunkRef>,
}

/// A stored version's content, used to restore a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredVersion {
    pub id: i64,
    pub path: PathBytes,
    pub size: u64,
    pub mtime_ns: i64,
    pub mode: u32,
    pub chunks: Vec<ChunkRef>,
}

/// An overwrite review row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverwriteRow {
    pub id: i64,
    pub folder_id: String,
    pub path: PathBytes,
    pub losing_version_id: i64,
    pub winning_version_id: i64,
    pub resolved: bool,
}

fn kind_str(kind: OpKind) -> &'static str {
    match kind {
        OpKind::Put => "put",
        OpKind::Delete => "delete",
        OpKind::Move => "move",
        OpKind::Resolve => "resolve",
    }
}

/// Apply one op on behalf of `device_id`, atomically.
pub fn apply_op(
    conn: &mut Connection,
    folder_id: &str,
    device_id: &str,
    op: &Op,
) -> Result<AppliedOp, StoreError> {
    if op.kind == OpKind::Move {
        // Move is delete+put at the client until it is implemented here.
        return Err(StoreError::Unsupported(
            "move ops are not implemented yet".to_string(),
        ));
    }

    let tx = conn.transaction()?;

    let chunks_json =
        serde_json::to_string(&op.chunks).map_err(|e| StoreError::Json(e.to_string()))?;

    tx.execute(
        "INSERT INTO ops (folder_id, device_id, base_seq, kind, path, base_version_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            folder_id,
            device_id,
            op.base_seq,
            kind_str(op.kind),
            op.path,
            op.base_version_id
        ],
    )?;
    let seq = tx.last_insert_rowid();

    let active: Option<i64> = tx
        .query_row(
            "SELECT active_version_id FROM files WHERE folder_id = ?1 AND path = ?2",
            params![folder_id, op.path],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten();

    let decision = Sequencer::decide(active, op.base_version_id);
    let is_delete = op.kind == OpKind::Delete;
    let (size, stored_chunks) = if is_delete {
        (0, "[]".to_string())
    } else {
        (op.size, chunks_json)
    };

    tx.execute(
        "INSERT INTO versions
           (folder_id, path, device_id, seq, base_version_id, size, mtime_ns, mode,
            chunks_json, restored_from)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            folder_id,
            op.path,
            device_id,
            seq,
            op.base_version_id,
            size as i64,
            op.mtime_ns,
            op.mode,
            stored_chunks,
            op.restored_from
        ],
    )?;
    let version_id = tx.last_insert_rowid();

    tx.execute(
        "INSERT INTO files (folder_id, path, active_version_id, deleted_at)
         VALUES (?1, ?2, ?3,
                 CASE WHEN ?4 THEN strftime('%Y-%m-%dT%H:%M:%fZ', 'now') ELSE NULL END)
         ON CONFLICT (folder_id, path) DO UPDATE
            SET active_version_id = excluded.active_version_id,
                deleted_at = excluded.deleted_at",
        params![folder_id, op.path, version_id, is_delete],
    )?;

    for chunk in &op.chunks {
        add_chunk_ref(&tx, &chunk.hash, chunk.len)?;
    }

    let overwrite = match decision {
        Decision::Overwrite {
            losing_version_id: Some(losing),
        } => {
            tx.execute(
                "INSERT INTO overwrites (folder_id, path, losing_version_id, winning_version_id)
                 VALUES (?1, ?2, ?3, ?4)",
                params![folder_id, op.path, losing, version_id],
            )?;
            Some(NewOverwrite {
                losing_version_id: losing,
                winning_version_id: version_id,
            })
        }
        _ => None,
    };

    tx.commit()?;
    Ok(AppliedOp {
        seq,
        version_id,
        overwrite,
    })
}

/// Record that a version references a chunk (create the row, or bump `refs`).
pub fn add_chunk_ref(conn: &Connection, hash: &str, size: u64) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO chunks (hash, size, refs) VALUES (?1, ?2, 1)
         ON CONFLICT (hash) DO UPDATE SET refs = refs + 1",
        params![hash, size as i64],
    )?;
    Ok(())
}

/// Note that a chunk's bytes are on disk, without claiming a reference yet.
pub fn ensure_chunk_row(conn: &Connection, hash: &str, size: u64) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO chunks (hash, size, refs) VALUES (?1, ?2, 0)
         ON CONFLICT (hash) DO NOTHING",
        params![hash, size as i64],
    )?;
    Ok(())
}

/// The current manifest of a folder — every path that is not deleted.
pub fn manifest(conn: &Connection, folder_id: &str) -> Result<Vec<ActiveFile>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT f.path, v.id, v.size, v.mtime_ns, v.mode, v.chunks_json
           FROM files f
           JOIN versions v ON v.id = f.active_version_id
          WHERE f.folder_id = ?1 AND f.deleted_at IS NULL
          ORDER BY f.path",
    )?;
    let raw = stmt
        .query_map([folder_id], |row| {
            Ok((
                row.get::<_, PathBytes>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)? as u64,
                row.get::<_, i64>(3)?,
                row.get::<_, u32>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    raw.into_iter()
        .map(|(path, version_id, size, mtime_ns, mode, chunks_json)| {
            Ok(ActiveFile {
                path,
                version_id,
                size,
                mtime_ns,
                mode,
                chunks: decode_chunks(&chunks_json)?,
            })
        })
        .collect()
}

/// The op log tail, joined with the content each op produced.
pub fn log_since(
    conn: &Connection,
    folder_id: &str,
    since: i64,
) -> Result<Vec<LogEntry>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT o.id, v.id, o.kind, o.path, o.device_id, v.size, v.mtime_ns, v.mode, v.chunks_json
           FROM ops o
           JOIN versions v ON v.folder_id = o.folder_id AND v.seq = o.id
          WHERE o.folder_id = ?1 AND o.id > ?2
          ORDER BY o.id",
    )?;
    let raw = stmt
        .query_map(params![folder_id, since], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, PathBytes>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)? as u64,
                row.get::<_, i64>(6)?,
                row.get::<_, u32>(7)?,
                row.get::<_, String>(8)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    raw.into_iter()
        .map(
            |(seq, version_id, kind, path, device_id, size, mtime_ns, mode, chunks_json)| {
                Ok(LogEntry {
                    seq,
                    version_id,
                    kind,
                    path,
                    device_id,
                    size,
                    mtime_ns,
                    mode,
                    chunks: decode_chunks(&chunks_json)?,
                })
            },
        )
        .collect()
}

/// Load one version's content (used to restore it).
pub fn version(conn: &Connection, version_id: i64) -> Result<Option<StoredVersion>, StoreError> {
    let row = conn
        .query_row(
            "SELECT id, path, size, mtime_ns, mode, chunks_json FROM versions WHERE id = ?1",
            [version_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, PathBytes>(1)?,
                    row.get::<_, i64>(2)? as u64,
                    row.get::<_, i64>(3)?,
                    row.get::<_, u32>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )
        .optional()?;

    match row {
        Some((id, path, size, mtime_ns, mode, chunks_json)) => Ok(Some(StoredVersion {
            id,
            path,
            size,
            mtime_ns,
            mode,
            chunks: decode_chunks(&chunks_json)?,
        })),
        None => Ok(None),
    }
}

/// The current active version id for a path, if the path exists and is not
/// deleted. Used as the base when the dashboard restores a version.
pub fn active_version_id(
    conn: &Connection,
    folder_id: &str,
    path: &[u8],
) -> Result<Option<i64>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT active_version_id FROM files WHERE folder_id = ?1 AND path = ?2",
            params![folder_id, path],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten())
}

/// The overwrite review queue, newest first.
pub fn overwrites(conn: &Connection, open_only: bool) -> Result<Vec<OverwriteRow>, StoreError> {
    let sql = if open_only {
        "SELECT id, folder_id, path, losing_version_id, winning_version_id, resolved_at
           FROM overwrites WHERE resolved_at IS NULL ORDER BY id DESC"
    } else {
        "SELECT id, folder_id, path, losing_version_id, winning_version_id, resolved_at
           FROM overwrites ORDER BY id DESC"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |row| {
        Ok(OverwriteRow {
            id: row.get(0)?,
            folder_id: row.get(1)?,
            path: row.get(2)?,
            losing_version_id: row.get(3)?,
            winning_version_id: row.get(4)?,
            resolved: row.get::<_, Option<String>>(5)?.is_some(),
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn decode_chunks(json: &str) -> Result<Vec<ChunkRef>, StoreError> {
    serde_json::from_str(json).map_err(|e| StoreError::Json(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use crate::folders;

    fn folder(store: &Store) -> folders::Folder {
        let folder = folders::Folder {
            id: "f1".into(),
            name: "Documents".into(),
            retention_versions: 50,
            retention_days: 90,
            encrypted: false,
        };
        store.with(|conn| folders::create(conn, &folder)).unwrap();
        folder
    }

    fn put_op(base_version_id: Option<i64>, chunks: Vec<ChunkRef>) -> Op {
        Op {
            base_seq: 0,
            base_version_id,
            kind: OpKind::Put,
            path: b"notes/todo.md".to_vec(),
            from: None,
            size: chunks.iter().map(|c| c.len).sum(),
            mtime_ns: 111,
            mode: 0o644,
            chunks,
            restored_from: None,
        }
    }

    #[test]
    fn a_first_put_creates_the_file_and_refs_its_chunks() {
        let store = Store::open_in_memory().unwrap();
        folder(&store);
        let chunks = vec![ChunkRef::new("aa", 10), ChunkRef::new("bb", 5)];

        let applied = store
            .with_mut(|conn| apply_op(conn, "f1", "dev-1", &put_op(None, chunks.clone())))
            .unwrap();
        assert_eq!(applied.seq, 1);
        assert!(applied.overwrite.is_none());

        let manifest = store.with(|conn| manifest(conn, "f1")).unwrap();
        assert_eq!(manifest.len(), 1);
        assert_eq!(manifest[0].path, b"notes/todo.md");
        assert_eq!(manifest[0].chunks, chunks);

        let refs: i64 = store
            .with(|conn| {
                Ok(
                    conn.query_row("SELECT refs FROM chunks WHERE hash = 'aa'", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .unwrap();
        assert_eq!(refs, 1);
    }

    #[test]
    fn a_stale_base_records_an_overwrite() {
        let store = Store::open_in_memory().unwrap();
        folder(&store);

        let first = store
            .with_mut(|conn| {
                apply_op(
                    conn,
                    "f1",
                    "dev-a",
                    &put_op(None, vec![ChunkRef::new("aa", 1)]),
                )
            })
            .unwrap();
        let second = store
            .with_mut(|conn| {
                // dev-b edits without having seen dev-a's version (base = None).
                apply_op(
                    conn,
                    "f1",
                    "dev-b",
                    &put_op(None, vec![ChunkRef::new("cc", 1)]),
                )
            })
            .unwrap();

        let overwrite = second.overwrite.expect("a stale base must be flagged");
        assert_eq!(overwrite.losing_version_id, first.version_id);
        assert_eq!(overwrite.winning_version_id, second.version_id);
        let open = store.with(|conn| overwrites(conn, true)).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].path, b"notes/todo.md");
    }

    #[test]
    fn a_delete_removes_the_path_from_the_manifest() {
        let store = Store::open_in_memory().unwrap();
        folder(&store);
        let applied = store
            .with_mut(|conn| {
                apply_op(
                    conn,
                    "f1",
                    "dev-1",
                    &put_op(None, vec![ChunkRef::new("aa", 1)]),
                )
            })
            .unwrap();
        let mut delete = put_op(Some(applied.seq), vec![]);
        delete.kind = OpKind::Delete;
        store
            .with_mut(|conn| apply_op(conn, "f1", "dev-1", &delete))
            .unwrap();
        assert!(store.with(|conn| manifest(conn, "f1")).unwrap().is_empty());
    }

    #[test]
    fn the_log_tail_is_ordered_and_joinable() {
        let store = Store::open_in_memory().unwrap();
        folder(&store);
        for i in 0..3 {
            store
                .with_mut(|conn| {
                    apply_op(
                        conn,
                        "f1",
                        "dev-1",
                        &put_op(None, vec![ChunkRef::new(format!("h{i}"), 1)]),
                    )
                })
                .unwrap();
        }
        let tail = store.with(|conn| log_since(conn, "f1", 1)).unwrap();
        assert_eq!(tail.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2, 3]);
        assert_eq!(tail[0].chunks, vec![ChunkRef::new("h1", 1)]);
    }
}
