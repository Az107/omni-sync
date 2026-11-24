//! Applying ops: the server's sequencer, in one transaction.
//!
//! Every op does four things together, or none of them:
//! 1. append to the op log (`ops`), which is what a polling client tails;
//! 2. create an immutable `versions` row (the content);
//! 3. point `files.active_version_id` at it (or mark the path deleted);
//! 4. if the sender's base was stale, insert an `overwrites` row for review.
//!
//! Chunk refcounts are bumped for anything the version references.

use std::collections::HashMap;

use omni_core::{ChunkRef, Decision, Op, OpKind, PathBytes, Sequencer};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::StoreError;

/// The longest path a client may submit. `PATH_MAX` on Linux is 4096 bytes
/// including the NUL, and a relative path under a watched root is never longer.
pub const MAX_PATH_BYTES: usize = 4096;

/// The largest single chunk the relay will accept or reference. The default
/// chunker caps at 1 MiB; this leaves room for a deliberately larger
/// configuration without letting a client claim an unbounded length.
pub const MAX_CHUNK_LEN: u64 = 64 * 1024 * 1024;

/// The most chunk refs one op may carry. The HTTP body limit is far smaller,
/// but a store-level bound keeps a hand-crafted op from exhausting memory.
pub const MAX_CHUNKS_PER_OP: usize = 1_048_576;

/// Is `hash` exactly 64 lowercase hex characters (the wire and storage shape of
/// a SHA-256 chunk id)?
pub fn is_valid_chunk_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Validate a path as it arrives from a client.
///
/// Paths are relative to the folder root and always `/`-separated. This mirrors
/// the receiving clients' rule (and `omni_core::validate_relative_path`), so
/// the server cannot accept a path a client would reject on apply: non-empty,
/// relative, no NUL, no backslash or colon (a Windows separator, drive prefix
/// or alternate-data-stream alias), and no empty, `.` or `..` segment. This
/// layer adds the [`MAX_PATH_BYTES`] bound.
///
/// This is syntax validation, not filesystem confinement: the client must still
/// apply relative to an opened directory so a symlink cannot escape the root.
pub fn validate_path(path: &[u8]) -> Result<(), StoreError> {
    if path.len() > MAX_PATH_BYTES {
        return Err(StoreError::InvalidOp(format!(
            "path is longer than {MAX_PATH_BYTES} bytes"
        )));
    }
    omni_core::validate_relative_path(path)
        .map_err(|error| StoreError::InvalidOp(error.to_string()))
}

/// Validate an op's shape before it can be committed: its path(s), the chunk
/// hash format, and metadata that must agree with the chunk list.
pub fn validate_op(op: &Op) -> Result<(), StoreError> {
    validate_path(&op.path)?;
    if op.kind == OpKind::Move {
        let from = op
            .from
            .as_deref()
            .ok_or_else(|| StoreError::InvalidOp("a move op requires a source path".into()))?;
        validate_path(from)?;
    }
    if op.kind == OpKind::Resolve && op.restored_from.is_none() {
        return Err(StoreError::InvalidOp(
            "a resolve op requires restored_from".into(),
        ));
    }
    if matches!(op.kind, OpKind::Put | OpKind::Resolve) {
        validate_content(op.size, &op.chunks)?;
    }
    Ok(())
}

fn validate_content(size: u64, chunks: &[ChunkRef]) -> Result<(), StoreError> {
    if size > i64::MAX as u64 {
        return Err(StoreError::InvalidOp("file size exceeds i64::MAX".into()));
    }
    if chunks.len() > MAX_CHUNKS_PER_OP {
        return Err(StoreError::InvalidOp(format!(
            "more than {MAX_CHUNKS_PER_OP} chunks in one op"
        )));
    }
    let mut total: u64 = 0;
    for chunk in chunks {
        if !is_valid_chunk_hash(&chunk.hash) {
            return Err(StoreError::InvalidOp(format!(
                "chunk hash {:?} is not 64 lowercase hex characters",
                chunk.hash
            )));
        }
        if chunk.len == 0 || chunk.len > MAX_CHUNK_LEN {
            return Err(StoreError::InvalidOp(format!(
                "chunk {} has an out-of-range length {}",
                chunk.hash, chunk.len
            )));
        }
        total = total
            .checked_add(chunk.len)
            .ok_or_else(|| StoreError::InvalidOp("chunk lengths overflow".into()))?;
        if total > i64::MAX as u64 {
            return Err(StoreError::InvalidOp("file size exceeds i64::MAX".into()));
        }
    }
    if total != size {
        return Err(StoreError::InvalidOp(format!(
            "chunk lengths total {total} but the op claims size {size}"
        )));
    }
    Ok(())
}

/// The stored sizes of the chunks named by `hashes`, keyed by hash. A hash that
/// has no row is absent from the map, which the caller treats as missing.
pub fn chunk_sizes(
    conn: &Connection,
    hashes: &[String],
) -> Result<HashMap<String, u64>, StoreError> {
    let mut stmt = conn.prepare("SELECT size FROM chunks WHERE hash = ?1")?;
    let mut sizes = HashMap::with_capacity(hashes.len());
    for hash in hashes {
        let size: Option<i64> = stmt.query_row([hash], |row| row.get(0)).optional()?;
        if let Some(size) = size {
            sizes.insert(hash.clone(), size as u64);
        }
    }
    Ok(sizes)
}

/// Reject an op that references a chunk row that does not exist or whose
/// recorded length disagrees with the op. The bytes on disk are verified by the
/// HTTP layer, which is the only layer that owns the chunk store.
fn check_chunk_rows(conn: &Connection, op: &Op) -> Result<(), StoreError> {
    // A delete's content fields are ignored by the commit path, so they must
    // not be able to fail validation either.
    if op.kind == OpKind::Delete || op.chunks.is_empty() {
        return Ok(());
    }
    let hashes: Vec<String> = op.chunks.iter().map(|chunk| chunk.hash.clone()).collect();
    let sizes = chunk_sizes(conn, &hashes)?;
    for chunk in &op.chunks {
        match sizes.get(&chunk.hash) {
            None => {
                return Err(StoreError::MissingChunk {
                    hash: chunk.hash.clone(),
                });
            }
            Some(size) if *size != chunk.len => {
                return Err(StoreError::InvalidOp(format!(
                    "chunk {} is stored with length {size}, op claims {}",
                    chunk.hash, chunk.len
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

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

/// A path whose file row is marked deleted, with the last content it had.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeletedFile {
    pub path: PathBytes,
    /// The newest version with content (`size > 0`) before the deletion.
    pub version_id: i64,
    pub size: u64,
    /// RFC 3339 UTC, milliseconds, trailing `Z`.
    pub deleted_at: String,
}

/// A stored version's content, used to restore a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredVersion {
    pub id: i64,
    /// The folder the version was committed to. A restore must never pull
    /// content from another folder's history.
    pub folder_id: String,
    pub path: PathBytes,
    pub size: u64,
    pub mtime_ns: i64,
    pub mode: u32,
    pub chunks: Vec<ChunkRef>,
}

/// One row of a path's version history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    pub version_id: i64,
    pub seq: i64,
    pub device_id: String,
    pub base_version_id: Option<i64>,
    pub size: u64,
    pub mtime_ns: i64,
    pub mode: u32,
    pub restored_from: Option<i64>,
    pub created_at: String,
    pub active: bool,
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

/// An overwrite with the metadata of both versions, for the dashboard's review
/// decision (which device made each version, when, and how big it is).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverwriteDetail {
    pub id: i64,
    pub folder_id: String,
    pub path: PathBytes,
    pub losing_version_id: i64,
    pub winning_version_id: i64,
    pub losing_device: String,
    pub losing_size: u64,
    pub losing_age_secs: i64,
    pub winning_device: String,
    pub winning_size: u64,
    pub winning_age_secs: i64,
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
    let mut applied = apply_ops(conn, folder_id, device_id, std::slice::from_ref(op))?;
    applied
        .pop()
        .ok_or_else(|| StoreError::InvalidOp("no op to apply".into()))
}

/// Apply a whole submitted batch in **one** transaction.
///
/// Every op is validated before the transaction opens, and every chunk a
/// version references is checked inside it. If any op is invalid or any chunk
/// is missing, the transaction is dropped and *nothing* from the batch is
/// committed — a client can retry the same batch after fixing the offending op
/// without having to reason about a half-applied prefix.
pub fn apply_ops(
    conn: &mut Connection,
    folder_id: &str,
    device_id: &str,
    ops: &[Op],
) -> Result<Vec<AppliedOp>, StoreError> {
    if ops.is_empty() {
        return Ok(Vec::new());
    }
    for op in ops {
        validate_op(op)?;
        if op.kind == OpKind::Move {
            // Move is delete+put at the client until it is implemented here.
            return Err(StoreError::Unsupported(
                "move ops are not implemented yet".to_string(),
            ));
        }
    }

    let tx = conn.transaction()?;
    let mut applied = Vec::with_capacity(ops.len());
    for op in ops {
        check_resolve_source(&tx, folder_id, op)?;
        check_chunk_rows(&tx, op)?;
        applied.push(apply_one(&tx, folder_id, device_id, op)?);
    }
    tx.commit()?;
    Ok(applied)
}

/// A resolve may only name a version from the folder it is being applied to.
/// This keeps the API's restore path and a hand-submitted `resolve` op to the
/// same rule.
fn check_resolve_source(conn: &Connection, folder_id: &str, op: &Op) -> Result<(), StoreError> {
    if op.kind != OpKind::Resolve {
        return Ok(());
    }
    let Some(source) = op.restored_from else {
        return Ok(());
    };
    let source_folder: Option<String> = conn
        .query_row(
            "SELECT folder_id FROM versions WHERE id = ?1",
            [source],
            |row| row.get(0),
        )
        .optional()?;
    match source_folder {
        Some(source_folder) if source_folder == folder_id => Ok(()),
        Some(_) => Err(StoreError::InvalidOp(format!(
            "version {source} belongs to another folder"
        ))),
        None => Err(StoreError::InvalidOp(format!(
            "unknown restored_from version {source}"
        ))),
    }
}

fn apply_one(
    tx: &Transaction<'_>,
    folder_id: &str,
    device_id: &str,
    op: &Op,
) -> Result<AppliedOp, StoreError> {
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

    if !is_delete {
        for chunk in &op.chunks {
            add_chunk_ref(tx, &chunk.hash, chunk.len)?;
        }
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

    Ok(AppliedOp {
        seq,
        version_id,
        overwrite,
    })
}

/// Record that a version references a chunk (create the row, or bump `refs`).
pub fn add_chunk_ref(conn: &Connection, hash: &str, size: u64) -> Result<(), StoreError> {
    check_chunk_metadata(hash, size)?;
    conn.execute(
        "INSERT INTO chunks (hash, size, refs) VALUES (?1, ?2, 1)
         ON CONFLICT (hash) DO UPDATE SET refs = refs + 1",
        params![hash, size as i64],
    )?;
    Ok(())
}

/// Note that a chunk's bytes are on disk, without claiming a reference yet.
pub fn ensure_chunk_row(conn: &Connection, hash: &str, size: u64) -> Result<(), StoreError> {
    check_chunk_metadata(hash, size)?;
    conn.execute(
        "INSERT INTO chunks (hash, size, refs) VALUES (?1, ?2, 0)
         ON CONFLICT (hash) DO NOTHING",
        params![hash, size as i64],
    )?;
    Ok(())
}

fn check_chunk_metadata(hash: &str, size: u64) -> Result<(), StoreError> {
    if !is_valid_chunk_hash(hash) {
        return Err(StoreError::InvalidOp(format!(
            "chunk hash {hash:?} is not 64 lowercase hex characters"
        )));
    }
    if size > i64::MAX as u64 {
        return Err(StoreError::InvalidOp("chunk size exceeds i64::MAX".into()));
    }
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

/// The highest op id in a folder: a member's `acked_seq` lags this.
pub fn folder_head(conn: &Connection, folder_id: &str) -> Result<i64, StoreError> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(id), 0) FROM ops WHERE folder_id = ?1",
        [folder_id],
        |row| row.get(0),
    )?)
}

/// The op log tail, joined with the content each op produced.
pub fn log_since(
    conn: &Connection,
    folder_id: &str,
    since: i64,
) -> Result<Vec<LogEntry>, StoreError> {
    Ok(log_since_limited(conn, folder_id, since, None)?.0)
}

/// The op log tail with the content each op produced, at most `limit` entries.
///
/// `limit == None` is the un-paginated tail every existing client gets: the
/// whole log from `since`. With a limit the query fetches one row more than it
/// returns, so `has_more` costs no second round trip. Returns
/// `(entries, has_more)`.
pub fn log_since_limited(
    conn: &Connection,
    folder_id: &str,
    since: i64,
    limit: Option<usize>,
) -> Result<(Vec<LogEntry>, bool), StoreError> {
    // `limit + 1` is what makes `has_more` answerable from this one query.
    let bound = limit.map(|limit| limit.saturating_add(1) as i64);
    let sql = if bound.is_some() {
        "SELECT o.id, v.id, o.kind, o.path, o.device_id, v.size, v.mtime_ns, v.mode, v.chunks_json
           FROM ops o
           JOIN versions v ON v.folder_id = o.folder_id AND v.seq = o.id
          WHERE o.folder_id = ?1 AND o.id > ?2
          ORDER BY o.id
          LIMIT ?3"
    } else {
        "SELECT o.id, v.id, o.kind, o.path, o.device_id, v.size, v.mtime_ns, v.mode, v.chunks_json
           FROM ops o
           JOIN versions v ON v.folder_id = o.folder_id AND v.seq = o.id
          WHERE o.folder_id = ?1 AND o.id > ?2
          ORDER BY o.id"
    };
    let mut stmt = conn.prepare(sql)?;
    let mut raw = if let Some(bound) = bound {
        stmt.query_map(params![folder_id, since, bound], log_row)?
            .collect::<Result<Vec<_>, _>>()?
    } else {
        stmt.query_map(params![folder_id, since], log_row)?
            .collect::<Result<Vec<_>, _>>()?
    };

    let has_more = limit.is_some_and(|limit| raw.len() > limit);
    if let Some(limit) = limit {
        raw.truncate(limit);
    }
    let entries = raw
        .into_iter()
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
        .collect::<Result<Vec<_>, StoreError>>()?;
    Ok((entries, has_more))
}

/// One raw `log_since` row, before the chunk JSON is decoded.
type RawLogRow = (i64, i64, String, PathBytes, String, u64, i64, u32, String);

fn log_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawLogRow> {
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
}

/// Every deleted path in a folder, newest deletion first.
///
/// One row per `files` row with `deleted_at` set. The version id is the most
/// recent version with content (the last bytes before the delete), because a
/// restore replays that content; paths that only ever held empty content have
/// no such version and are skipped. A path re-created after its delete has
/// `deleted_at = NULL` and so is not here at all.
pub fn deleted_files(conn: &Connection, folder_id: &str) -> Result<Vec<DeletedFile>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT f.path, v.id, v.size, f.deleted_at
           FROM files f
           JOIN versions v ON v.id = (
                SELECT id FROM versions
                 WHERE folder_id = f.folder_id AND path = f.path AND size > 0
                 ORDER BY id DESC
                 LIMIT 1
           )
          WHERE f.folder_id = ?1 AND f.deleted_at IS NOT NULL
          ORDER BY f.deleted_at DESC, f.path",
    )?;
    let rows = stmt.query_map([folder_id], |row| {
        Ok(DeletedFile {
            path: row.get::<_, PathBytes>(0)?,
            version_id: row.get(1)?,
            size: row.get::<_, i64>(2)? as u64,
            deleted_at: row.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Load one version's content (used to restore it).
pub fn version(conn: &Connection, version_id: i64) -> Result<Option<StoredVersion>, StoreError> {
    let row = conn
        .query_row(
            "SELECT id, folder_id, path, size, mtime_ns, mode, chunks_json
               FROM versions WHERE id = ?1",
            [version_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, PathBytes>(2)?,
                    row.get::<_, i64>(3)? as u64,
                    row.get::<_, i64>(4)?,
                    row.get::<_, u32>(5)?,
                    row.get::<_, String>(6)?,
                ))
            },
        )
        .optional()?;

    match row {
        Some((id, folder_id, path, size, mtime_ns, mode, chunks_json)) => Ok(Some(StoredVersion {
            id,
            folder_id,
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

/// One overwrite review row by id.
pub fn overwrite(conn: &Connection, id: i64) -> Result<Option<OverwriteRow>, StoreError> {
    conn.query_row(
        "SELECT id, folder_id, path, losing_version_id, winning_version_id, resolved_at
           FROM overwrites WHERE id = ?1",
        [id],
        |row| {
            Ok(OverwriteRow {
                id: row.get(0)?,
                folder_id: row.get(1)?,
                path: row.get(2)?,
                losing_version_id: row.get(3)?,
                winning_version_id: row.get(4)?,
                resolved: row.get::<_, Option<String>>(5)?.is_some(),
            })
        },
    )
    .optional()
    .map_err(StoreError::from)
}

/// Mark an overwrite review row resolved (after a restore or dismiss).
pub fn mark_overwrite_resolved(conn: &Connection, id: i64) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE overwrites
            SET resolved_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
          WHERE id = ?1 AND resolved_at IS NULL",
        [id],
    )?;
    Ok(())
}

/// Mark every open overwrite row for `(folder_id, path)` resolved.
pub fn mark_path_overwrites_resolved(
    conn: &Connection,
    folder_id: &str,
    path: &[u8],
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE overwrites
            SET resolved_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
          WHERE folder_id = ?1 AND path = ?2 AND resolved_at IS NULL",
        params![folder_id, path],
    )?;
    Ok(())
}

/// The version chain for one path in a folder, newest first.
pub fn history(
    conn: &Connection,
    folder_id: &str,
    path: &[u8],
) -> Result<Vec<HistoryEntry>, StoreError> {
    let active = active_version_id(conn, folder_id, path)?;
    let mut stmt = conn.prepare(
        "SELECT v.id, v.seq, COALESCE(d.name, v.device_id), v.base_version_id,
                v.size, v.mtime_ns, v.mode, v.restored_from, v.created_at
           FROM versions v
           LEFT JOIN devices d ON d.id = v.device_id
          WHERE v.folder_id = ?1 AND v.path = ?2
          ORDER BY v.id DESC",
    )?;
    let rows = stmt.query_map(params![folder_id, path], |row| {
        let version_id: i64 = row.get(0)?;
        Ok(HistoryEntry {
            version_id,
            seq: row.get(1)?,
            device_id: row.get(2)?,
            base_version_id: row.get(3)?,
            size: row.get::<_, i64>(4)? as u64,
            mtime_ns: row.get(5)?,
            mode: row.get(6)?,
            restored_from: row.get(7)?,
            created_at: row.get(8)?,
            active: active == Some(version_id),
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// The overwrite review queue, newest first.
pub fn overwrites(conn: &Connection, open_only: bool) -> Result<Vec<OverwriteRow>, StoreError> {
    overwrites_since(conn, open_only, 0)
}

/// The overwrite review queue with `id > since`, newest first.
pub fn overwrites_since(
    conn: &Connection,
    open_only: bool,
    since: i64,
) -> Result<Vec<OverwriteRow>, StoreError> {
    let sql = if open_only {
        "SELECT id, folder_id, path, losing_version_id, winning_version_id, resolved_at
           FROM overwrites WHERE resolved_at IS NULL AND id > ?1 ORDER BY id DESC"
    } else {
        "SELECT id, folder_id, path, losing_version_id, winning_version_id, resolved_at
           FROM overwrites WHERE id > ?1 ORDER BY id DESC"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([since], |row| {
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

/// The review queue with each version's device, size and age, newest first.
/// Ages are computed in SQLite so the dashboard needs no date parsing.
pub fn overwrites_detailed(
    conn: &Connection,
    open_only: bool,
) -> Result<Vec<OverwriteDetail>, StoreError> {
    let filter = if open_only {
        "WHERE o.resolved_at IS NULL"
    } else {
        ""
    };
    let sql = format!(
        "SELECT o.id, o.folder_id, o.path, o.losing_version_id, o.winning_version_id,
                COALESCE(dl.name, lv.device_id), lv.size,
                CAST(strftime('%s', 'now') AS INTEGER) - CAST(strftime('%s', lv.created_at) AS INTEGER),
                COALESCE(dw.name, wv.device_id), wv.size,
                CAST(strftime('%s', 'now') AS INTEGER) - CAST(strftime('%s', wv.created_at) AS INTEGER)
           FROM overwrites o
           JOIN versions lv ON lv.id = o.losing_version_id
           JOIN versions wv ON wv.id = o.winning_version_id
           LEFT JOIN devices dl ON dl.id = lv.device_id
           LEFT JOIN devices dw ON dw.id = wv.device_id
           {filter}
          ORDER BY o.id DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok(OverwriteDetail {
            id: row.get(0)?,
            folder_id: row.get(1)?,
            path: row.get(2)?,
            losing_version_id: row.get(3)?,
            winning_version_id: row.get(4)?,
            losing_device: row.get(5)?,
            losing_size: row.get::<_, i64>(6)? as u64,
            losing_age_secs: row.get(7)?,
            winning_device: row.get(8)?,
            winning_size: row.get::<_, i64>(9)? as u64,
            winning_age_secs: row.get(10)?,
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
    use crate::folders;

    fn folder(store: &Store) -> folders::Folder {
        let folder = folders::Folder {
            id: "f1".into(),
            name: "Documents".into(),
            retention_versions: 50,
            retention_days: 90,
            encrypted: false,
            exclude: Vec::new(),
            preserve_permissions: true,
        };
        store.with(|conn| folders::create(conn, &folder)).unwrap();
        folder
    }

    /// A valid 64-character lowercase-hex hash derived from one byte.
    fn chunk_hash(byte: u8) -> String {
        format!("{byte:02x}").repeat(32)
    }

    fn chunk(byte: u8, len: u64) -> ChunkRef {
        ChunkRef::new(chunk_hash(byte), len)
    }

    fn seed_chunks(store: &Store, chunks: &[ChunkRef]) {
        store
            .with(|conn| {
                for chunk in chunks {
                    ensure_chunk_row(conn, &chunk.hash, chunk.len)?;
                }
                Ok(())
            })
            .unwrap();
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

    fn apply(store: &Store, device: &str, op: &Op) -> Result<AppliedOp, StoreError> {
        seed_chunks(store, &op.chunks);
        store.with_mut(|conn| apply_op(conn, "f1", device, op))
    }

    #[test]
    fn a_first_put_creates_the_file_and_refs_its_chunks() {
        let store = Store::open_in_memory().unwrap();
        folder(&store);
        let chunks = vec![chunk(b'a', 10), chunk(b'b', 5)];

        let applied = apply(&store, "dev-1", &put_op(None, chunks.clone())).unwrap();
        assert_eq!(applied.seq, 1);
        assert!(applied.overwrite.is_none());

        let manifest = store.with(|conn| manifest(conn, "f1")).unwrap();
        assert_eq!(manifest.len(), 1);
        assert_eq!(manifest[0].path, b"notes/todo.md");
        assert_eq!(manifest[0].chunks, chunks);

        let hash = chunk_hash(b'a');
        let refs: i64 = store
            .with(|conn| {
                Ok(
                    conn.query_row("SELECT refs FROM chunks WHERE hash = ?1", [&hash], |r| {
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

        let first = apply(&store, "dev-a", &put_op(None, vec![chunk(b'a', 1)])).unwrap();
        // dev-b edits without having seen dev-a's version (base = None).
        let second = apply(&store, "dev-b", &put_op(None, vec![chunk(b'c', 1)])).unwrap();

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
        let applied = apply(&store, "dev-1", &put_op(None, vec![chunk(b'a', 1)])).unwrap();
        let mut delete = put_op(Some(applied.seq), vec![]);
        delete.kind = OpKind::Delete;
        apply(&store, "dev-1", &delete).unwrap();
        assert!(store.with(|conn| manifest(conn, "f1")).unwrap().is_empty());
    }

    #[test]
    fn the_log_tail_is_ordered_and_joinable() {
        let store = Store::open_in_memory().unwrap();
        folder(&store);
        for i in 0..3 {
            let op = put_op(None, vec![chunk(i + 1, 1)]);
            apply(&store, "dev-1", &op).unwrap();
        }
        let tail = store.with(|conn| log_since(conn, "f1", 1)).unwrap();
        assert_eq!(tail.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2, 3]);
        assert_eq!(tail[0].chunks, vec![chunk(2, 1)]);
    }

    #[test]
    fn a_limited_log_tail_pages_and_reports_more() {
        let store = Store::open_in_memory().unwrap();
        folder(&store);
        for i in 0..3 {
            let op = put_op(None, vec![chunk(i + 1, 1)]);
            apply(&store, "dev-1", &op).unwrap();
        }

        // A page asks for one row more than it returns to answer `has_more`.
        let (page, more) = store
            .with(|conn| log_since_limited(conn, "f1", 0, Some(2)))
            .unwrap();
        assert_eq!(page.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2]);
        assert!(more);

        // The follow-up page is short: the log is exhausted.
        let (page, more) = store
            .with(|conn| log_since_limited(conn, "f1", 2, Some(2)))
            .unwrap();
        assert_eq!(page.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![3]);
        assert!(!more);

        // No limit is the old un-paginated behaviour, and has_more is false.
        let (page, more) = store
            .with(|conn| log_since_limited(conn, "f1", 0, None))
            .unwrap();
        assert_eq!(page.len(), 3);
        assert!(!more);

        // An empty page still has no more to give.
        let (page, more) = store
            .with(|conn| log_since_limited(conn, "f1", 3, Some(10)))
            .unwrap();
        assert!(page.is_empty());
        assert!(!more);
    }

    #[test]
    fn deleted_files_carry_the_last_content_version() {
        let store = Store::open_in_memory().unwrap();
        folder(&store);

        // Content, then an empty file that has nothing worth restoring.
        let todo = apply(&store, "dev-1", &put_op(None, vec![chunk(b'a', 7)])).unwrap();
        let other = apply(
            &store,
            "dev-1",
            &Op {
                path: b"notes/other.md".to_vec(),
                size: 3,
                ..put_op(None, vec![chunk(b'c', 3)])
            },
        )
        .unwrap();
        let empty = apply(
            &store,
            "dev-1",
            &Op {
                path: b"empty.md".to_vec(),
                size: 0,
                ..put_op(None, Vec::new())
            },
        )
        .unwrap();

        // Delete all three, then pin the delete times so ordering is
        // deterministic (millisecond timestamps could otherwise tie).
        for (seq, path) in [
            (todo.seq, b"notes/todo.md".to_vec()),
            (other.seq, b"notes/other.md".to_vec()),
            (empty.seq, b"empty.md".to_vec()),
        ] {
            let delete = Op {
                base_seq: seq,
                base_version_id: Some(seq),
                kind: OpKind::Delete,
                path,
                from: None,
                size: 0,
                mtime_ns: 0,
                mode: 0,
                chunks: Vec::new(),
                restored_from: None,
            };
            apply(&store, "dev-1", &delete).unwrap();
        }
        store
            .with(|conn| {
                for (path, at) in [
                    (&b"empty.md"[..], "2026-01-01T00:00:00.000Z"),
                    (&b"notes/other.md"[..], "2026-01-15T00:00:00.000Z"),
                    (&b"notes/todo.md"[..], "2026-02-01T00:00:00.000Z"),
                ] {
                    conn.execute(
                        "UPDATE files SET deleted_at = ?2 WHERE path = ?1",
                        params![path, at],
                    )?;
                }
                Ok(())
            })
            .unwrap();

        // `empty.md` had no content version, so it is skipped; the rest are
        // newest deletion first and point at the put that held the bytes.
        let deleted = store.with(|conn| deleted_files(conn, "f1")).unwrap();
        assert_eq!(deleted.len(), 2);
        assert_eq!(deleted[0].path, b"notes/todo.md");
        assert_eq!(deleted[0].version_id, todo.version_id);
        assert_eq!(deleted[0].size, 7);
        assert_eq!(deleted[0].deleted_at, "2026-02-01T00:00:00.000Z");
        assert_eq!(deleted[1].path, b"notes/other.md");
        assert_eq!(deleted[1].version_id, other.version_id);
        assert_eq!(deleted[1].size, 3);

        // Re-creating the path clears `deleted_at`, so it leaves the trash;
        // the other deleted path is untouched.
        apply(&store, "dev-1", &put_op(None, vec![chunk(b'b', 3)])).unwrap();
        let deleted = store.with(|conn| deleted_files(conn, "f1")).unwrap();
        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0].path, b"notes/other.md");
    }

    #[test]
    fn paths_are_relative_and_well_formed() {
        for valid in [
            &b"notes/todo.md"[..],
            b"r\xc3\xa9sum\xc3\xa9.pdf",
            b"dir with spaces/file.txt",
            b".hidden",
        ] {
            validate_path(valid)
                .unwrap_or_else(|e| panic!("{:?}: {e}", String::from_utf8_lossy(valid)));
        }

        for invalid in [
            &b""[..],
            b"/etc/passwd",
            b"../escape",
            b"notes/../../escape",
            b"notes/./todo.md",
            b"notes//todo.md",
            b"notes/",
            b"notes\\todo.md",
            b"\\\\server\\share",
            b"C:evil",
            b"C:\\Windows\\evil",
            b"dir/file:stream",
            b"nul\\0byte",
        ] {
            assert!(
                validate_path(invalid).is_err(),
                "should reject {:?}",
                String::from_utf8_lossy(invalid)
            );
        }

        // The length bound is enforced, not just the shape.
        let too_long = vec![b'a'; MAX_PATH_BYTES + 1];
        assert!(validate_path(&too_long).is_err());
    }

    #[test]
    fn op_metadata_must_match_its_chunks() {
        // A well-formed op passes.
        validate_op(&put_op(None, vec![chunk(b'a', 5), chunk(b'b', 7)])).unwrap();

        // A short, uppercase or non-hex hash is refused.
        for hash in ["aa".to_string(), "AA".repeat(32), "zz".repeat(32)] {
            let op = put_op(None, vec![ChunkRef::new(hash, 1)]);
            assert!(matches!(validate_op(&op), Err(StoreError::InvalidOp(_))));
        }

        // Chunk lengths must total the claimed size.
        let mut mismatched = put_op(None, vec![chunk(b'a', 5)]);
        mismatched.size = 4;
        assert!(matches!(
            validate_op(&mismatched),
            Err(StoreError::InvalidOp(_))
        ));

        // A zero-length or over-large chunk is refused.
        for len in [0, MAX_CHUNK_LEN + 1] {
            let op = put_op(None, vec![chunk(b'a', len)]);
            assert!(matches!(validate_op(&op), Err(StoreError::InvalidOp(_))));
        }

        // A delete only needs a valid path; its content fields are ignored.
        let mut delete = put_op(None, vec![]);
        delete.kind = OpKind::Delete;
        validate_op(&delete).unwrap();

        // A resolve must name the version it restores.
        let mut resolve = put_op(None, vec![]);
        resolve.kind = OpKind::Resolve;
        assert!(matches!(
            validate_op(&resolve),
            Err(StoreError::InvalidOp(_))
        ));
    }

    #[test]
    fn a_missing_chunk_is_rejected_and_nothing_commits() {
        let store = Store::open_in_memory().unwrap();
        folder(&store);
        // Deliberately do not seed the chunk row.
        let op = put_op(None, vec![chunk(b'a', 1)]);
        let error = store
            .with_mut(|conn| apply_op(conn, "f1", "dev-1", &op))
            .unwrap_err();
        assert!(matches!(error, StoreError::MissingChunk { .. }));
        assert_eq!(store.with(|conn| folder_head(conn, "f1")).unwrap(), 0);
        assert!(store.with(|conn| manifest(conn, "f1")).unwrap().is_empty());
    }

    #[test]
    fn a_resolve_cannot_reference_another_folders_version() {
        let store = Store::open_in_memory().unwrap();
        folder(&store);
        store
            .with(|conn| folders::create(conn, &folders::Folder::new("f2", "Other")))
            .unwrap();
        let source = apply(&store, "dev-1", &put_op(None, vec![chunk(b'a', 1)])).unwrap();
        seed_chunks(&store, &[chunk(b'a', 1)]);

        let resolve = Op {
            base_seq: 0,
            base_version_id: None,
            kind: OpKind::Resolve,
            path: b"b.txt".to_vec(),
            from: None,
            size: 1,
            mtime_ns: 1,
            mode: 0o644,
            chunks: vec![chunk(b'a', 1)],
            restored_from: Some(source.version_id),
        };
        let error = store
            .with_mut(|conn| apply_op(conn, "f2", "dev-1", &resolve))
            .unwrap_err();
        assert!(matches!(error, StoreError::InvalidOp(_)));
        assert_eq!(store.with(|conn| folder_head(conn, "f2")).unwrap(), 0);

        // The same op in the owning folder is fine.
        store
            .with_mut(|conn| apply_op(conn, "f1", "dev-1", &resolve))
            .unwrap();
        assert_eq!(store.with(|conn| folder_head(conn, "f1")).unwrap(), 2);
    }

    #[test]
    fn a_batch_with_one_bad_op_commits_nothing() {
        let store = Store::open_in_memory().unwrap();
        folder(&store);
        seed_chunks(&store, &[chunk(b'a', 1)]);

        let good = put_op(None, vec![chunk(b'a', 1)]);
        let missing = Op {
            path: b"notes/second.md".to_vec(),
            ..put_op(None, vec![chunk(b'z', 1)])
        };
        let error = store
            .with_mut(|conn| apply_ops(conn, "f1", "dev-1", &[good, missing]))
            .unwrap_err();
        assert!(matches!(error, StoreError::MissingChunk { .. }));

        // The first op must not have been committed either.
        assert_eq!(store.with(|conn| folder_head(conn, "f1")).unwrap(), 0);
        assert!(store.with(|conn| manifest(conn, "f1")).unwrap().is_empty());
    }
}
