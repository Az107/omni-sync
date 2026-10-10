//! Conversions between the wire DTOs and `omni-core`'s domain types.
//!
//! Kept in one module so the mapping between "what JSON carries" and "what the
//! rules operate on" is greppable; the two type sets stay deliberately separate.

use omni_core::{ChunkRef, FileMeta, Op, OpKind, PathBytes};

use crate::{ChunkRefDto, ManifestEntryDto, OpDto, OpKindDto};

/// Core chunk refs → wire.
pub fn encode_chunks(chunks: &[ChunkRef]) -> Vec<ChunkRefDto> {
    chunks
        .iter()
        .map(|chunk| ChunkRefDto {
            hash: chunk.hash.clone(),
            len: chunk.len,
        })
        .collect()
}

/// Wire chunk refs → core. Values are trusted structurally; the *hash* is
/// verified against bytes elsewhere, never here.
pub fn decode_chunks(chunks: &[ChunkRefDto]) -> Vec<ChunkRef> {
    chunks
        .iter()
        .map(|chunk| ChunkRef::new(chunk.hash.clone(), chunk.len))
        .collect()
}

/// A manifest entry → the core metadata for that file.
pub fn decode_file_meta(entry: &ManifestEntryDto) -> FileMeta {
    FileMeta {
        size: entry.size,
        mtime_ns: entry.mtime_ns,
        mode: entry.mode,
        chunks: decode_chunks(&entry.chunks),
    }
}

impl OpKindDto {
    pub fn from_core(kind: OpKind) -> Self {
        match kind {
            OpKind::Put => OpKindDto::Put,
            OpKind::Delete => OpKindDto::Delete,
            OpKind::Move => OpKindDto::Move,
            OpKind::Resolve => OpKindDto::Resolve,
        }
    }

    pub fn to_core(self) -> OpKind {
        match self {
            OpKindDto::Put => OpKind::Put,
            OpKindDto::Delete => OpKind::Delete,
            OpKindDto::Move => OpKind::Move,
            OpKindDto::Resolve => OpKind::Resolve,
        }
    }
}

impl OpDto {
    /// Encode a core op for submission.
    pub fn from_core(op: &Op) -> Self {
        Self {
            base_seq: op.base_seq,
            base_version_id: op.base_version_id,
            kind: OpKindDto::from_core(op.kind),
            path: crate::encode_path(&op.path),
            from: op.from.as_deref().map(crate::encode_path),
            size: op.size,
            mtime_ns: op.mtime_ns,
            mode: op.mode,
            chunks: encode_chunks(&op.chunks),
            restored_from: op.restored_from,
        }
    }

    /// Decode a submitted op, or fail on an unreadable path.
    pub fn to_core(&self) -> Result<Op, base64::DecodeError> {
        let from = match &self.from {
            Some(encoded) => Some(crate::decode_path(encoded)?),
            None => None,
        };
        Ok(Op {
            base_seq: self.base_seq,
            base_version_id: self.base_version_id,
            kind: self.kind.to_core(),
            path: crate::decode_path(&self.path)?,
            from,
            size: self.size,
            mtime_ns: self.mtime_ns,
            mode: self.mode,
            chunks: decode_chunks(&self.chunks),
            restored_from: self.restored_from,
        })
    }
}

/// Convenience for tests and the client: the raw path bytes of an entry.
pub fn entry_path(entry: &ManifestEntryDto) -> Result<PathBytes, base64::DecodeError> {
    crate::decode_path(&entry.path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_core_op_round_trips_through_its_dto() {
        let op = Op {
            base_seq: 7,
            base_version_id: Some(3),
            kind: OpKind::Put,
            path: b"notes/todo.md".to_vec(),
            from: None,
            size: 12,
            mtime_ns: 42,
            mode: 0o644,
            chunks: vec![ChunkRef::new("deadbeef", 12)],
            restored_from: None,
        };
        let dto = OpDto::from_core(&op);
        assert_eq!(dto.to_core().unwrap(), op);
    }

    #[test]
    fn chunks_round_trip() {
        let chunks = vec![ChunkRef::new("aa", 1), ChunkRef::new("bb", 2)];
        assert_eq!(decode_chunks(&encode_chunks(&chunks)), chunks);
    }
}
