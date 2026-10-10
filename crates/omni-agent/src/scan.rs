//! Walking a folder into a [`Manifest`].
//!
//! Paths are relative to the watched root and kept as raw bytes. Files the
//! client should never sync (its own temp files, editor droppings) are skipped
//! here, so they never enter a diff.

use std::fs;
use std::io;
use std::path::Path;
use std::time::UNIX_EPOCH;

use omni_chunk::{Chunker, HashedChunk};
use omni_core::{ChunkRef, FileMeta, Manifest, PathBytes};

/// Scan `root` recursively into a manifest.
///
/// `previous` is the last-synced manifest. A file whose size, mtime and mode
/// match an entry there is **not read**: its chunk list is reused. That is the
/// "metadata is triage" rule — we hash only what looks changed.
///
/// The cost of the shortcut is that a file edited in place with its timestamp
/// preserved will be missed until its metadata changes. A periodic full-content
/// re-verify would close that hole at the price of reading the whole tree; it is
/// deliberately not wired into the steady-state loop, and belongs in an
/// explicit `omni verify` command instead.
pub fn scan(root: &Path, chunker: &Chunker, previous: &Manifest) -> io::Result<Manifest> {
    let mut manifest = Manifest::new();
    walk(root, root, chunker, previous, &mut manifest)?;
    Ok(manifest)
}

/// Read one file, chunk and hash it. Returns the metadata and the chunk bytes,
/// so a push can upload exactly the content it reported.
pub fn chunk_file(
    root: &Path,
    rel: &[u8],
    chunker: &Chunker,
) -> io::Result<(FileMeta, Vec<HashedChunk>)> {
    let path = root.join(path_from_bytes(rel));
    let bytes = fs::read(&path)?;
    let meta = fs::metadata(&path)?;

    let hashed = chunker.chunk_and_hash(&bytes);
    let chunks: Vec<ChunkRef> = hashed
        .iter()
        .map(|chunk| ChunkRef::new(chunk.hash.clone(), chunk.len))
        .collect();

    let file_meta = FileMeta {
        size: bytes.len() as u64,
        mtime_ns: mtime_ns(&meta),
        mode: mode(&meta),
        chunks,
    };
    Ok((file_meta, hashed))
}

fn walk(
    root: &Path,
    dir: &Path,
    chunker: &Chunker,
    previous: &Manifest,
    manifest: &mut Manifest,
) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if is_ignored(&name.to_string_lossy()) {
            continue;
        }
        let path = entry.path();
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            walk(root, &path, chunker, previous, manifest)?;
        } else if file_type.is_file() {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let meta = fs::metadata(&path)?;
            let mtime = mtime_ns(&meta);
            let mode = mode(&meta);

            // Fast path: identical metadata to last sync → reuse the known
            // chunks without reading a byte.
            if let Some(known) = previous.get(&path_bytes(rel))
                && known.size == meta.len()
                && known.mtime_ns == mtime
                && known.mode == mode
            {
                manifest.insert(path_bytes(rel), known.clone());
                continue;
            }

            let bytes = fs::read(&path)?;
            let chunks: Vec<ChunkRef> = chunker
                .chunk_and_hash(&bytes)
                .into_iter()
                .map(|chunk| ChunkRef::new(chunk.hash, chunk.len))
                .collect();
            manifest.insert(
                path_bytes(rel),
                FileMeta {
                    size: bytes.len() as u64,
                    mtime_ns: mtime,
                    mode,
                    chunks,
                },
            );
        }
        // Symlinks and other special files are skipped: following them risks
        // loops and syncing things outside the root.
    }
    Ok(())
}

/// Names the client never syncs.
///
/// Being-written files (`.part`, `.crdownload`, editor swap files) would be
/// captured half-written otherwise, and the client's own temp suffix must never
/// be re-ingested.
pub fn is_ignored(name: &str) -> bool {
    name == ".omni-sync"
        || name == ".omniignore"
        || name == ".DS_Store"
        || name == "Thumbs.db"
        || name.ends_with(".omni-tmp")
        || name.ends_with(".part")
        || name.ends_with(".crdownload")
        || name.starts_with("~$")
        || (name.starts_with('.') && name.ends_with(".swp"))
}

/// A path's raw bytes, relative to the root.
#[cfg(unix)]
pub fn path_bytes(path: &Path) -> PathBytes {
    use std::os::unix::ffi::OsStrExt as _;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
pub fn path_bytes(path: &Path) -> PathBytes {
    path.to_string_lossy().into_owned().into_bytes()
}

/// Raw relative bytes back to a path under `root`.
#[cfg(unix)]
pub fn path_from_bytes(bytes: &[u8]) -> std::path::PathBuf {
    use std::os::unix::ffi::OsStrExt as _;
    std::path::PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
pub fn path_from_bytes(bytes: &[u8]) -> std::path::PathBuf {
    std::path::PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

fn mtime_ns(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn mode(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn mode(_meta: &fs::Metadata) -> u32 {
    0o644
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply;
    use omni_chunk::{Chunker, Config};

    #[test]
    fn scan_then_apply_reproduces_the_manifest() {
        let chunker = Chunker::new(Config::small());
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();

        std::fs::create_dir_all(source.path().join("sub")).unwrap();
        std::fs::write(source.path().join("a.txt"), b"hello world").unwrap();
        std::fs::write(source.path().join("sub/b.bin"), vec![7u8; 5_000]).unwrap();
        // Ignored names never enter a manifest.
        std::fs::write(source.path().join(".DS_Store"), b"junk").unwrap();
        std::fs::write(source.path().join("wip.part"), b"partial").unwrap();

        let manifest = scan(source.path(), &chunker, &Manifest::new()).unwrap();
        assert_eq!(manifest.len(), 2, "ignored files must be skipped");

        // Write each file into the target using the pulled bytes.
        for (rel, meta) in &manifest {
            let (_, hashed) = chunk_file(source.path(), rel, &chunker).unwrap();
            let mut bytes = Vec::new();
            for chunk in &hashed {
                bytes.extend_from_slice(&chunk.data);
            }
            apply::write_file(target.path(), rel, &bytes, meta).unwrap();
        }

        let reapplied = scan(target.path(), &chunker, &Manifest::new()).unwrap();
        assert_eq!(
            reapplied.keys().collect::<Vec<_>>(),
            manifest.keys().collect::<Vec<_>>(),
            "the same paths must exist on both sides"
        );
        for (path, meta) in &manifest {
            // mtime is not restored on apply, so compare content + mode.
            assert!(
                reapplied.get(path).unwrap().equivalent(meta),
                "content mismatch for {:?}",
                String::from_utf8_lossy(path)
            );
        }
    }

    #[test]
    fn removing_a_missing_file_is_not_an_error() {
        let root = tempfile::tempdir().unwrap();
        apply::remove_file(root.path(), b"never-existed").unwrap();
    }

    #[test]
    fn an_unchanged_file_is_not_read_again() {
        use omni_chunk::hash_hex;

        let chunker = Chunker::new(Config::small());
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), b"hello world").unwrap();

        // A "previous" manifest with correct metadata but deliberately wrong
        // chunks. If the fast path works, the scan reuses those chunks without
        // reading the file — proving it did not hash it.
        let real = scan(root.path(), &chunker, &Manifest::new()).unwrap();
        let mut poisoned = real.clone();
        poisoned.insert(
            b"a.txt".to_vec(),
            FileMeta {
                size: 11,
                mtime_ns: real.get(b"a.txt".as_slice()).unwrap().mtime_ns,
                mode: real.get(b"a.txt".as_slice()).unwrap().mode,
                chunks: vec![ChunkRef::new("poisoned", 11)],
            },
        );

        let rescanned = scan(root.path(), &chunker, &poisoned).unwrap();
        assert_eq!(
            rescanned.get(b"a.txt".as_slice()).unwrap().chunks,
            vec![ChunkRef::new("poisoned", 11)],
            "unchanged metadata must short-circuit the read"
        );

        // Sanity: a real read would have produced the true hash.
        assert_ne!(hash_hex(b"hello world"), "poisoned");
    }
}
