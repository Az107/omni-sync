//! The relay chunk store: content-addressed bytes on disk.
//!
//! `chunks/<hash[0..2]>/<hash>`. Bytes are verified against the hash they are
//! stored under before anything is written, so a corrupt or lying client cannot
//! poison the store.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ChunkError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("chunk hash mismatch: expected {expected}, computed {actual}")]
    Mismatch { expected: String, actual: String },
    #[error("invalid chunk hash {0:?}")]
    InvalidHash(String),
}

/// A content-addressed store rooted at one directory.
#[derive(Clone)]
pub struct ChunkStore {
    root: PathBuf,
}

impl ChunkStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Is the chunk's bytes present?
    pub fn contains(&self, hash: &str) -> bool {
        self.path_for(hash).map(|p| p.exists()).unwrap_or(false)
    }

    /// Store `bytes` under `hash`, verifying them first.
    ///
    /// Returns `Ok(true)` when newly stored and `Ok(false)` when already
    /// present (the idempotent case the client relies on for resume).
    pub fn put(&self, hash: &str, bytes: &[u8]) -> Result<bool, ChunkError> {
        let path = self.path_for(hash)?;
        if path.exists() {
            return Ok(false);
        }
        let actual = omni_chunk::hash_hex(bytes);
        if actual != hash {
            return Err(ChunkError::Mismatch {
                expected: hash.to_string(),
                actual,
            });
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Write to a sibling temp file and rename, so a crash never leaves a
        // partial chunk under a valid name.
        let tmp = path.with_extension("part");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(true)
    }

    /// Read a chunk's bytes, or `None` if it is absent.
    pub fn get(&self, hash: &str) -> Result<Option<Vec<u8>>, ChunkError> {
        let path = self.path_for(hash)?;
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn path_for(&self, hash: &str) -> Result<PathBuf, ChunkError> {
        let valid = hash.len() >= 2
            && hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !valid {
            return Err(ChunkError::InvalidHash(hash.to_string()));
        }
        Ok(self.root.join(&hash[..2]).join(hash))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, ChunkStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ChunkStore::new(dir.path().join("chunks"));
        (dir, store)
    }

    #[test]
    fn stored_bytes_round_trip_and_are_idempotent() {
        let (_dir, store) = temp_store();
        let bytes = b"hello world";
        let hash = omni_chunk::hash_hex(bytes);

        assert!(!store.contains(&hash));
        assert!(store.put(&hash, bytes).unwrap(), "first put stores");
        assert!(!store.put(&hash, bytes).unwrap(), "second put is a no-op");
        assert!(store.contains(&hash));
        assert_eq!(store.get(&hash).unwrap().unwrap(), bytes);
    }

    #[test]
    fn bytes_that_do_not_match_their_hash_are_rejected() {
        let (_dir, store) = temp_store();
        let hash = omni_chunk::hash_hex(b"the real bytes");
        let error = store.put(&hash, b"different bytes").unwrap_err();
        assert!(matches!(error, ChunkError::Mismatch { .. }));
        assert!(!store.contains(&hash), "nothing is stored on a mismatch");
    }

    #[test]
    fn a_traversal_shaped_hash_is_refused() {
        let (_dir, store) = temp_store();
        assert!(matches!(
            store.get("../etc/passwd"),
            Err(ChunkError::InvalidHash(_))
        ));
        assert!(matches!(
            store.get("ABCDEF"),
            Err(ChunkError::InvalidHash(_))
        ));
    }

    #[test]
    fn a_missing_chunk_is_none_not_an_error() {
        let (_dir, store) = temp_store();
        let hash = omni_chunk::hash_hex(b"absent");
        assert!(store.get(&hash).unwrap().is_none());
    }
}
