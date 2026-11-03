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
use omni_core::{ChunkRef, FileMeta, Ignore, Manifest, PathBytes};

/// How a scan should treat the folder's policy.
#[derive(Clone)]
pub struct ScanOptions {
    /// Folder-level excludes, shared with every other client.
    pub ignore: Ignore,
    /// When false, report `mode = 0` for every file. The plugin/mobile client
    /// cannot express POSIX modes, and `FileMeta::equivalent` compares mode, so
    /// a folder shared with it must agree on a single sentinel.
    pub preserve_permissions: bool,
}

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
pub fn scan(
    root: &Path,
    chunker: &Chunker,
    previous: &Manifest,
    options: ScanOptions,
) -> io::Result<Manifest> {
    let mut manifest = Manifest::new();
    walk(root, root, chunker, previous, &options, &mut manifest)?;
    Ok(manifest)
}

/// Read one file, chunk and hash it. Returns the metadata and the chunk bytes,
/// so a push can upload exactly the content it reported.
pub fn chunk_file(
    root: &Path,
    rel: &[u8],
    chunker: &Chunker,
    preserve_permissions: bool,
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
        mode: mode(&meta, preserve_permissions),
        chunks,
    };
    Ok((file_meta, hashed))
}

#[allow(clippy::too_many_arguments)]
fn walk(
    root: &Path,
    dir: &Path,
    chunker: &Chunker,
    previous: &Manifest,
    options: &ScanOptions,
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
        let rel = path.strip_prefix(root).unwrap_or(&path);
        let rel_bytes = path_bytes(rel);
        if options.ignore.matches(&rel_bytes) {
            continue;
        }

        if file_type.is_dir() {
            walk(root, &path, chunker, previous, options, manifest)?;
        } else if file_type.is_file() {
            let meta = fs::metadata(&path)?;
            let mtime = mtime_ns(&meta);
            let mode = mode(&meta, options.preserve_permissions);

            // Fast path: identical metadata to last sync → reuse the known
            // chunks without reading a byte.
            if let Some(known) = previous.get(&rel_bytes)
                && known.size == meta.len()
                && known.mtime_ns == mtime
                && known.mode == mode
            {
                manifest.insert(rel_bytes, known.clone());
                continue;
            }

            let bytes = fs::read(&path)?;
            let chunks: Vec<ChunkRef> = chunker
                .chunk_and_hash(&bytes)
                .into_iter()
                .map(|chunk| ChunkRef::new(chunk.hash, chunk.len))
                .collect();
            manifest.insert(
                rel_bytes,
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
fn mode(meta: &fs::Metadata, preserve_permissions: bool) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    if preserve_permissions {
        meta.permissions().mode() & 0o7777
    } else {
        0
    }
}

#[cfg(not(unix))]
fn mode(_meta: &fs::Metadata, preserve_permissions: bool) -> u32 {
    if preserve_permissions { 0o644 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply;
    use omni_chunk::{Chunker, Config};

    fn options(ignore: &Ignore, preserve_permissions: bool) -> ScanOptions {
        ScanOptions {
            ignore: ignore.clone(),
            preserve_permissions,
        }
    }

    #[test]
    fn scan_then_apply_reproduces_the_manifest() {
        let chunker = Chunker::new(Config::small());
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let ignore = Ignore::default();

        std::fs::create_dir_all(source.path().join("sub")).unwrap();
        std::fs::write(source.path().join("a.txt"), b"hello world").unwrap();
        std::fs::write(source.path().join("sub/b.bin"), vec![7u8; 5_000]).unwrap();
        // Ignored names never enter a manifest.
        std::fs::write(source.path().join(".DS_Store"), b"junk").unwrap();
        std::fs::write(source.path().join("wip.part"), b"partial").unwrap();

        let manifest = scan(
            source.path(),
            &chunker,
            &Manifest::new(),
            options(&ignore, true),
        )
        .unwrap();
        assert_eq!(manifest.len(), 2, "ignored files must be skipped");

        // Write each file into the target using the pulled bytes.
        for (rel, meta) in &manifest {
            let (_, hashed) = chunk_file(source.path(), rel, &chunker, true).unwrap();
            let mut bytes = Vec::new();
            for chunk in &hashed {
                bytes.extend_from_slice(&chunk.data);
            }
            apply::write_file(target.path(), rel, &bytes, meta).unwrap();
        }

        let reapplied = scan(
            target.path(),
            &chunker,
            &Manifest::new(),
            options(&ignore, true),
        )
        .unwrap();
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
    fn excluded_paths_never_enter_the_manifest() {
        let chunker = Chunker::new(Config::small());
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".obsidian/cache")).unwrap();
        std::fs::create_dir_all(root.path().join("notes")).unwrap();
        std::fs::write(root.path().join(".obsidian/workspace.json"), b"{}").unwrap();
        std::fs::write(root.path().join(".obsidian/cache/index"), b"junk").unwrap();
        std::fs::write(root.path().join(".obsidian/appearance.json"), b"{}").unwrap();
        std::fs::write(root.path().join("notes/todo.md"), b"hello").unwrap();

        let ignore = Ignore::vault_defaults();
        let manifest = scan(
            root.path(),
            &chunker,
            &Manifest::new(),
            options(&ignore, false),
        )
        .unwrap();

        let paths: Vec<String> = manifest
            .keys()
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect();
        assert_eq!(paths, vec![".obsidian/appearance.json", "notes/todo.md"]);
    }

    #[test]
    fn permissions_are_reported_as_zero_when_the_folder_does_not_preserve_them() {
        let chunker = Chunker::new(Config::small());
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.sh"), b"#!/bin/sh\n").unwrap();
        let ignore = Ignore::default();

        // Both platforms report 0, so a browser client and a unix daemon agree.
        let manifest = scan(
            root.path(),
            &chunker,
            &Manifest::new(),
            options(&ignore, false),
        )
        .unwrap();
        assert_eq!(manifest.get(b"a.sh".as_slice()).unwrap().mode, 0);
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
        let ignore = Ignore::default();
        std::fs::write(root.path().join("a.txt"), b"hello world").unwrap();

        // A "previous" manifest with correct metadata but deliberately wrong
        // chunks. If the fast path works, the scan reuses those chunks without
        // reading the file — proving it did not hash it.
        let real = scan(
            root.path(),
            &chunker,
            &Manifest::new(),
            options(&ignore, true),
        )
        .unwrap();
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

        let rescanned = scan(root.path(), &chunker, &poisoned, options(&ignore, true)).unwrap();
        assert_eq!(
            rescanned.get(b"a.txt".as_slice()).unwrap().chunks,
            vec![ChunkRef::new("poisoned", 11)],
            "unchanged metadata must short-circuit the read"
        );

        // Sanity: a real read would have produced the true hash.
        assert_ne!(hash_hex(b"hello world"), "poisoned");
    }
}
