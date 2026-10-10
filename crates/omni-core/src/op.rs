//! The operation a client submits, and the pieces it references.

use crate::version::PathBytes;

/// A lowercase-hex SHA-256 of a chunk's bytes.
pub type ChunkHash = String;

/// One chunk of a file: its content hash and its length in bytes.
///
/// The offset is *not* stored: a file's chunks are ordered, and offset is the
/// running sum of the lengths, so storing it would be a second source of truth
/// for the same number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkRef {
    pub hash: ChunkHash,
    pub len: u64,
}

impl ChunkRef {
    pub fn new(hash: impl Into<String>, len: u64) -> Self {
        Self {
            hash: hash.into(),
            len,
        }
    }
}

/// What an op does to a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpKind {
    /// Create or replace the file at `path` with `chunks`.
    Put,
    /// Remove the file at `path`.
    Delete,
    /// Move `from` to `path`.
    Move,
    /// Restore the content of `restored_from` at `path` (a dashboard rollback).
    Resolve,
}

/// A change to a folder, as submitted by a client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Op {
    /// The `applied_seq` the sender had when the change happened. The server
    /// compares this with the active version to detect an overwrite.
    pub base_seq: i64,
    pub kind: OpKind,
    /// Destination path for `Put`/`Move`/`Resolve`; the removed path for `Delete`.
    pub path: PathBytes,
    /// Source path for `Move`.
    pub from: Option<PathBytes>,
    pub size: u64,
    pub mtime_ns: i64,
    pub mode: u32,
    /// Ordered chunks of the new content (`Put`/`Resolve`).
    pub chunks: Vec<ChunkRef>,
    /// The version a `Resolve` restores.
    pub restored_from: Option<i64>,
}
