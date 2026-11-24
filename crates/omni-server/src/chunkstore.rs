//! The relay chunk store: content-addressed bytes on disk.
//!
//! `chunks/<hash[0..2]>/<hash>`. Bytes are verified against the hash they are
//! stored under before anything is written, so a corrupt or lying client cannot
//! poison the store.
//!
//! Writes use an exclusively-created, randomly named temp file in the target
//! directory, `fsync` it, rename it over the final name, then `fsync` the
//! directory. Two clients uploading the same chunk therefore never share a
//! `.part` file, a crash never leaves a truncated chunk under a valid name, and
//! the rename is durable before the upload is acknowledged.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use omni_store::sync::{MAX_CHUNK_LEN, is_valid_chunk_hash};

#[derive(Debug, thiserror::Error)]
pub enum ChunkError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("chunk hash mismatch: expected {expected}, computed {actual}")]
    Mismatch { expected: String, actual: String },
    #[error("invalid chunk hash {0:?}")]
    InvalidHash(String),
    #[error("chunk of {size} bytes exceeds the {max}-byte limit")]
    TooLarge { size: u64, max: u64 },
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
        matches!(self.len(hash), Ok(Some(_)))
    }

    /// The on-disk length of a stored chunk, or `None` when it is absent.
    ///
    /// This is the server's proof that a version's chunk list points at real
    /// bytes of the claimed length, not just at a row in `chunks`.
    pub fn len(&self, hash: &str) -> Result<Option<u64>, ChunkError> {
        let path = self.path_for(hash)?;
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() => Ok(Some(meta.len())),
            Ok(_) => Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Store `bytes` under `hash`, verifying them first.
    ///
    /// Returns `Ok(true)` when newly stored and `Ok(false)` when already
    /// present (the idempotent case the client relies on for resume).
    pub fn put(&self, hash: &str, bytes: &[u8]) -> Result<bool, ChunkError> {
        let path = self.path_for(hash)?;
        let size = bytes.len() as u64;
        if size > MAX_CHUNK_LEN {
            return Err(ChunkError::TooLarge {
                size,
                max: MAX_CHUNK_LEN,
            });
        }
        // Verify the body even when the chunk already exists: the store never
        // accepts bytes under a hash they were not proven to match, and a
        // corrupt re-upload is a client error rather than a silent no-op.
        let actual = omni_chunk::hash_hex(bytes);
        if actual != hash {
            return Err(ChunkError::Mismatch {
                expected: hash.to_string(),
                actual,
            });
        }
        if path.exists() {
            return Ok(false);
        }
        let parent = path
            .parent()
            .ok_or_else(|| ChunkError::InvalidHash(hash.to_string()))?;
        std::fs::create_dir_all(parent)?;
        sync_dir(&self.root)?;
        if let Some(root_parent) = self
            .root
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            sync_dir(root_parent)?;
        }

        // `create_new` makes the temp name exclusive: concurrent uploads of the
        // same chunk get distinct files and cannot truncate each other.
        let tmp = temp_path(parent, hash);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        let write = (|| -> std::io::Result<()> {
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(())
        })();
        drop(file);
        if let Err(error) = write {
            let _ = std::fs::remove_file(&tmp);
            return Err(error.into());
        }

        match std::fs::rename(&tmp, &path) {
            Ok(()) => {}
            // Another upload won the race with identical, hash-verified bytes.
            Err(_) if path.exists() => {
                let _ = std::fs::remove_file(&tmp);
                return Ok(false);
            }
            Err(error) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(error.into());
            }
        }
        // Make the rename itself durable. A failure here is a real I/O problem,
        // not a reason to pretend the bytes are safely stored.
        sync_dir(parent)?;
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
        if !is_valid_chunk_hash(hash) {
            return Err(ChunkError::InvalidHash(hash.to_string()));
        }
        Ok(self.root.join(&hash[..2]).join(hash))
    }
}

/// A temp name in `parent`, unique per process and per call.
fn temp_path(parent: &Path, hash: &str) -> PathBuf {
    parent.join(format!(
        ".{hash}.tmp.{}.{:016x}",
        std::process::id(),
        rand::random::<u64>()
    ))
}

/// Flush a directory entry so a rename survives a power loss.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
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
        assert_eq!(store.len(&hash).unwrap(), Some(bytes.len() as u64));
        assert_eq!(store.get(&hash).unwrap().unwrap(), bytes);
    }

    #[test]
    fn bytes_that_do_not_match_their_hash_are_rejected() {
        let (_dir, store) = temp_store();
        let hash = omni_chunk::hash_hex(b"the real bytes");
        let error = store.put(&hash, b"different bytes").unwrap_err();
        assert!(matches!(error, ChunkError::Mismatch { .. }));
        assert!(!store.contains(&hash), "nothing is stored on a mismatch");

        // A re-upload under an existing hash is still verified, so a corrupt
        // body cannot ride in on the idempotent path.
        store.put(&hash, b"the real bytes").unwrap();
        let error = store.put(&hash, b"different bytes").unwrap_err();
        assert!(matches!(error, ChunkError::Mismatch { .. }));
    }

    #[test]
    fn only_exact_lowercase_sha256_hashes_are_paths() {
        let (_dir, store) = temp_store();
        let valid = omni_chunk::hash_hex(b"x");
        assert_eq!(valid.len(), 64);
        store.get(&valid).unwrap();

        for invalid in [
            String::new(),
            "a".to_string(),
            "aa".to_string(),
            valid[..63].to_string(),
            format!("{valid}a"),
            valid.to_uppercase(),
            "../etc/passwd".to_string(),
            "zz".repeat(32),
        ] {
            assert!(
                matches!(store.get(&invalid), Err(ChunkError::InvalidHash(_))),
                "should reject {invalid:?}"
            );
        }
    }

    #[test]
    fn concurrent_identical_uploads_share_no_temp_file() {
        let (dir, store) = temp_store();
        let bytes = b"concurrent chunk bytes";
        let hash = omni_chunk::hash_hex(bytes);

        let threads: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
                let hash = hash.clone();
                std::thread::spawn(move || store.put(&hash, bytes))
            })
            .collect();
        for thread in threads {
            // Either "I stored it" or "someone else did" is correct; neither
            // may fail or see a truncated file.
            thread.join().unwrap().unwrap();
        }

        assert_eq!(store.get(&hash).unwrap().unwrap(), bytes);
        // No `.tmp.*` leftovers beside the final chunk.
        let parent = dir.path().join("chunks").join(&hash[..2]);
        let leftovers: Vec<_> = std::fs::read_dir(&parent)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name != hash.as_str())
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }

    #[test]
    fn a_missing_chunk_is_none_not_an_error() {
        let (_dir, store) = temp_store();
        let hash = omni_chunk::hash_hex(b"absent");
        assert!(store.get(&hash).unwrap().is_none());
        assert_eq!(store.len(&hash).unwrap(), None);
    }
}
