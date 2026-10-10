//! Applying pulled content to disk, atomically.

use std::fs;
use std::io;
use std::path::Path;
use std::time::UNIX_EPOCH;

use omni_core::FileMeta;

use crate::scan::path_from_bytes;

/// The current on-disk mtime of a file, in nanoseconds.
///
/// The engine records this after applying a pull, so the *next* scan's metadata
/// fast path matches and does not re-read the file it just wrote.
pub fn local_mtime_ns(root: &Path, rel: &[u8]) -> io::Result<i64> {
    let meta = fs::metadata(root.join(path_from_bytes(rel)))?;
    Ok(meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0))
}

/// Write `bytes` at `rel` under `root`, via a temp file and a rename.
///
/// A reader of the destination never sees a half-written file: the rename is
/// atomic. The file's mode is restored; mtime is not (it needs `utimensat` and a
/// libc dependency that is not worth it yet — content, not mtime, decides sync).
pub fn write_file(root: &Path, rel: &[u8], bytes: &[u8], meta: &FileMeta) -> io::Result<()> {
    let path = root.join(path_from_bytes(rel));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Same directory as the target, so the rename stays within one filesystem.
    let tmp = temp_path(&path);
    fs::write(&tmp, bytes)?;
    set_mode(&tmp, meta.mode)?;
    // If the rename fails, do not leave the temp file behind.
    if let Err(error) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(())
}

/// Remove a file, treating "already gone" as success (idempotent replay).
pub fn remove_file(root: &Path, rel: &[u8]) -> io::Result<()> {
    let path = root.join(path_from_bytes(rel));
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn temp_path(path: &Path) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".omni-tmp");
    path.with_file_name(name)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}
