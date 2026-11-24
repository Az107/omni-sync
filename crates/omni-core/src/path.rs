//! Syntax checks shared by the server and receiving clients.
//!
//! This is not filesystem confinement: callers still need directory-relative
//! operations that cannot follow a symlink out of the configured root.

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidPath;

impl fmt::Display for InvalidPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected a non-empty relative path with normal components")
    }
}

impl std::error::Error for InvalidPath {}

/// Validate a `/`-separated sync path without converting its raw bytes to text.
///
/// Backslashes and colons are rejected on every platform: accepting them on a
/// Unix sender would turn the same wire path into separators, a drive prefix,
/// or an alternate data stream on Windows. Non-UTF-8 names remain valid here;
/// a receiver unable to represent one must reject it rather than rename it via
/// a lossy conversion.
pub fn validate_relative_path(path: &[u8]) -> Result<(), InvalidPath> {
    if path.is_empty()
        || path.iter().any(|byte| matches!(byte, 0 | b'\\' | b':'))
        || path
            .split(|byte| *byte == b'/')
            .any(|part| part.is_empty() || part == b"." || part == b"..")
    {
        return Err(InvalidPath);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal_prefixes_and_windows_aliases() {
        for path in [
            b"".as_slice(),
            b"/absolute",
            b"../outside",
            b"a/../../outside",
            b"a/./b",
            b"a//b",
            b"a/",
            b"C:relative",
            b"C:/absolute",
            b"\\\\server\\share",
            b"a\\..\\b",
            b"dir/file:stream",
            b"dir/\0file",
        ] {
            assert_eq!(validate_relative_path(path), Err(InvalidPath), "{path:?}");
        }
    }

    #[test]
    fn accepts_relative_paths_without_reencoding_bytes() {
        for path in [
            b"note".as_slice(),
            b"notes/todo.md",
            b"dir/\xff\xfe",
            b"a/.../b",
        ] {
            assert_eq!(validate_relative_path(path), Ok(()), "{path:?}");
        }
    }
}
