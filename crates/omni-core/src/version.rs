//! Versions: every state a path has ever had.

use crate::op::ChunkRef;

/// A filesystem path as raw bytes.
///
/// Not a `String`. A Linux filename is not guaranteed to be UTF-8, and coercing
/// it (lossy `to_string_lossy`) silently renames files across devices. The API
/// boundary base64-encodes these bytes; the database stores them as a `BLOB`.
pub type PathBytes = Vec<u8>;

/// One immutable version of one path.
///
/// Versions are append-only. Last-write-wins never deletes one — it supersedes
/// it — which is what lets the dashboard offer a rollback after the fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    /// The database id, absent until the server commits it.
    pub id: Option<i64>,
    pub folder_id: String,
    pub path: PathBytes,
    pub device_id: String,
    /// The active version the sender had seen when it made the change.
    pub base_version_id: Option<i64>,
    pub size: u64,
    pub mtime_ns: i64,
    pub mode: u32,
    /// Ordered chunks of this version's content. For a tombstone, empty.
    pub chunks: Vec<ChunkRef>,
    /// When set, this version restores the content of that version.
    pub restored_from: Option<i64>,
}

impl Version {
    /// A version is a tombstone when it carries no content — a delete.
    pub fn is_tombstone(&self) -> bool {
        self.chunks.is_empty()
    }
}
