//! Root-capability-relative apply operations.

use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use omni_core::FileMeta;
use std::{
    io,
    path::{Component, Path, PathBuf},
};

pub fn validate_rel(rel: &[u8]) -> io::Result<PathBuf> {
    omni_core::validate_relative_path(rel)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    #[cfg(windows)]
    validate_windows_name(rel)?;
    let path = crate::scan::path_from_bytes(rel);
    if path
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsafe relative path",
        ));
    }
    Ok(path)
}

#[cfg(windows)]
fn validate_windows_name(rel: &[u8]) -> io::Result<()> {
    let text = std::str::from_utf8(rel)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "unrepresentable path"))?;
    for component in text.split('/') {
        let stem = component
            .split('.')
            .next()
            .unwrap_or_default()
            .trim_end_matches(' ')
            .to_ascii_uppercase();
        if component.ends_with(['.', ' '])
            || component
                .chars()
                .any(|c| c < ' ' || matches!(c, '<' | '>' | '"' | '|' | '?' | '*'))
            || matches!(
                stem.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
            )
            || stem
                .strip_prefix("COM")
                .or_else(|| stem.strip_prefix("LPT"))
                .is_some_and(|suffix| {
                    matches!(
                        suffix,
                        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                    )
                })
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ambiguous Windows path",
            ));
        }
    }
    Ok(())
}

fn parent(root: &Path, rel: &[u8], create: bool) -> io::Result<(Dir, PathBuf)> {
    let path = validate_rel(rel)?;
    let mut dir = Dir::open_ambient_dir(root, ambient_authority())?;
    let components: Vec<_> = path.components().collect();
    for component in &components[..components.len() - 1] {
        let name = component.as_os_str();
        match dir.open_dir(name) {
            Ok(next) => dir = next,
            Err(e) if create && e.kind() == io::ErrorKind::NotFound => {
                dir.create_dir(name)?;
                sync_dir(&dir)?;
                dir = dir.open_dir(name)?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok((dir, PathBuf::from(components.last().unwrap().as_os_str())))
}

#[cfg(test)]
pub fn read_file(root: &Path, rel: &[u8]) -> io::Result<Vec<u8>> {
    let (dir, leaf) = parent(root, rel, false)?;
    dir.read(leaf)
}
pub fn open_file(root: &Path, rel: &[u8]) -> io::Result<std::fs::File> {
    let (dir, leaf) = parent(root, rel, false)?;
    Ok(dir.open(leaf)?.into_std())
}
pub fn metadata(root: &Path, rel: &[u8]) -> io::Result<std::fs::Metadata> {
    let (dir, leaf) = parent(root, rel, false)?;
    dir.open(leaf)?.into_std().metadata()
}

pub fn local_mtime_ns(root: &Path, rel: &[u8]) -> io::Result<i64> {
    let (dir, leaf) = parent(root, rel, false)?;
    let meta = dir.open(leaf)?.into_std().metadata()?;
    Ok(meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0))
}

pub fn write_file(root: &Path, rel: &[u8], bytes: &[u8], meta: &FileMeta) -> io::Result<()> {
    let (dir, leaf) = parent(root, rel, true)?;
    let preserved = if meta.mode == 0 {
        match dir.symlink_metadata(&leaf) {
            Ok(existing) if existing.is_file() => Some(existing.permissions()),
            Ok(_) => None,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                #[cfg(unix)]
                {
                    Some(default_file_permissions(&dir)?)
                }
                #[cfg(not(unix))]
                {
                    None
                }
            }
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    let tmp = PathBuf::from(format!(".omni-{:032x}.omni-tmp", rand::random::<u128>()));
    let result = (|| {
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let mut file = dir.open_with(&tmp, &opts)?;
        use std::io::Write as _;
        file.write_all(bytes)?;
        if meta.mode != 0 {
            set_mode(&file, meta.mode)?;
        } else if let Some(permissions) = preserved {
            file.set_permissions(permissions)?;
        }
        file.sync_all()?;
        drop(file);
        dir.rename(&tmp, &dir, &leaf)?;
        // Directory fsync is supported on Unix; Windows does not allow opening
        // directories as files and relies on the rename semantics there.
        sync_dir(&dir)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = dir.remove_file(&tmp);
    }
    result
}

#[cfg(unix)]
fn default_file_permissions(dir: &Dir) -> io::Result<cap_std::fs::Permissions> {
    use cap_std::fs::OpenOptionsExt as _;
    // Discover the directory/umask default with an empty probe, not by briefly
    // making the content-bearing temp public or changing the process umask.
    let probe = format!(".omni-mode-{:032x}.omni-tmp", rand::random::<u128>());
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o666);
    let file = dir.open_with(&probe, &options)?;
    let permissions = file.metadata().map(|meta| meta.permissions());
    drop(file);
    let cleanup = dir.remove_file(&probe);
    let permissions = permissions?;
    cleanup?;
    Ok(permissions)
}

pub fn remove_file(root: &Path, rel: &[u8]) -> io::Result<()> {
    let (dir, leaf) = match parent(root, rel, false) {
        Ok(parts) => parts,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    match dir.remove_file(leaf) {
        Ok(()) => sync_dir(&dir),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn sync_dir(dir: &Dir) -> io::Result<()> {
    // Opening `.` as a file yields an fsync-able descriptor, unlike the O_PATH
    // descriptor backing a Linux Dir, without returning to ambient paths.
    #[cfg(unix)]
    dir.open(".")?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

#[cfg(unix)]
fn set_mode(file: &cap_std::fs::File, mode: u32) -> io::Result<()> {
    use cap_std::fs::PermissionsExt as _;
    let mut permissions = file.metadata()?.permissions();
    permissions.set_mode(mode);
    file.set_permissions(permissions)
}
#[cfg(not(unix))]
fn set_mode(_file: &cap_std::fs::File, _mode: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn meta() -> FileMeta {
        FileMeta {
            size: 1,
            mtime_ns: 0,
            mode: 0,
            chunks: vec![],
        }
    }
    #[test]
    fn rejects_unsafe_paths() {
        for p in [&b"../x"[..], b"/x", b"C:foo", b"a\\b", b"a//b"] {
            assert!(validate_rel(p).is_err(), "{p:?}");
        }
    }
    #[cfg(unix)]
    #[test]
    fn symlink_parent_cannot_escape_capability() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("link")).unwrap();
        assert!(write_file(root.path(), b"link/x", b"x", &meta()).is_err());
        assert!(!outside.path().join("x").exists());
    }
    #[test]
    fn temp_suffix_is_scanner_ignored_and_apply_round_trips() {
        let root = tempfile::tempdir().unwrap();
        write_file(root.path(), b"f", b"x", &meta()).unwrap();
        assert_eq!(read_file(root.path(), b"f").unwrap(), b"x");
        assert!(crate::scan::is_ignored("f-123-2.omni-tmp"));
    }

    #[cfg(unix)]
    #[test]
    fn replacing_a_private_file_with_mode_zero_keeps_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("f"), b"private").unwrap();
        std::fs::set_permissions(
            root.path().join("f"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        write_file(root.path(), b"f", b"x", &meta()).unwrap();
        assert_eq!(
            std::fs::metadata(root.path().join("f"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_new_mode_zero_file_uses_the_normal_umask_default() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("normal"), b"").unwrap();
        let expected = std::fs::metadata(root.path().join("normal"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        write_file(root.path(), b"f", b"x", &meta()).unwrap();
        assert_eq!(
            std::fs::metadata(root.path().join("f"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            expected
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_legacy_temp_symlink_is_never_opened() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("victim");
        std::fs::write(&victim, b"private").unwrap();
        std::os::unix::fs::symlink(&victim, root.path().join("f.omni-tmp")).unwrap();
        write_file(root.path(), b"f", b"x", &meta()).unwrap();
        assert_eq!(std::fs::read(victim).unwrap(), b"private");
        assert_eq!(read_file(root.path(), b"f").unwrap(), b"x");
        assert!(remove_file(root.path(), b"missing/sub/file").is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn windows_rejects_aliases_and_lossy_paths() {
        for path in [b"NUL.txt".as_slice(), b"dir/COM1", b"a.", b"a ", b"a/\xff"] {
            assert!(validate_rel(path).is_err(), "{path:?}");
        }
    }
}
