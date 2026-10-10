//! Manifests and the diff between two of them.
//!
//! Sync needs three views of a folder: **current** (what is on disk now),
//! **last synced** (what this device believes is committed) and **remote**
//! (what the server reports). The interesting computation is the same one
//! twice:
//!
//! - current vs last-synced → changes to **push**;
//! - remote vs last-synced → changes to **pull**.
//!
//! [`changes`] is that one function. Everything here is pure.

use std::collections::BTreeMap;

use crate::op::ChunkRef;
use crate::version::PathBytes;

/// What this device knows about one file's content.
///
/// Equality for sync purposes is content (the chunk list) plus the mode bit — a
/// file whose only change is its mtime is not a change worth a round trip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMeta {
    pub size: u64,
    pub mtime_ns: i64,
    pub mode: u32,
    pub chunks: Vec<ChunkRef>,
}

impl FileMeta {
    /// Two metas describe the same synced content.
    pub fn equivalent(&self, other: &Self) -> bool {
        self.chunks == other.chunks && self.mode == other.mode
    }
}

/// A folder's files keyed by raw path. Ordered, so diffs and output are stable.
pub type Manifest = BTreeMap<PathBytes, FileMeta>;

/// A change to apply, in the direction "make `base` look like `next`".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Put { path: PathBytes, meta: FileMeta },
    Delete { path: PathBytes },
}

/// The changes that turn `base` into `next`.
///
/// A path present in `next` with different content is a `Put`; a path in `base`
/// that `next` no longer has is a `Delete`. This is deliberately symmetric so
/// the same function serves both the push and the pull direction.
pub fn changes(base: &Manifest, next: &Manifest) -> Vec<Change> {
    let mut out = Vec::new();

    for (path, meta) in next {
        match base.get(path) {
            Some(known) if known.equivalent(meta) => {}
            _ => out.push(Change::Put {
                path: path.clone(),
                meta: meta.clone(),
            }),
        }
    }

    for path in base.keys() {
        if !next.contains_key(path) {
            out.push(Change::Delete { path: path.clone() });
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(chunks: &[(&str, u64)], mode: u32) -> FileMeta {
        FileMeta {
            size: chunks.iter().map(|(_, len)| len).sum(),
            mtime_ns: 1_000,
            mode,
            chunks: chunks
                .iter()
                .map(|(hash, len)| ChunkRef::new(*hash, *len))
                .collect(),
        }
    }

    fn path(s: &str) -> PathBytes {
        s.as_bytes().to_vec()
    }

    #[test]
    fn an_added_file_is_a_put() {
        let base = Manifest::new();
        let mut next = Manifest::new();
        next.insert(path("a.txt"), meta(&[("h1", 10)], 0o644));
        assert_eq!(
            changes(&base, &next),
            vec![Change::Put {
                path: path("a.txt"),
                meta: meta(&[("h1", 10)], 0o644),
            }]
        );
    }

    #[test]
    fn a_removed_file_is_a_delete() {
        let mut base = Manifest::new();
        base.insert(path("a.txt"), meta(&[("h1", 10)], 0o644));
        let next = Manifest::new();
        assert_eq!(
            changes(&base, &next),
            vec![Change::Delete {
                path: path("a.txt")
            }]
        );
    }

    #[test]
    fn new_content_is_a_put_and_identical_content_is_nothing() {
        let mut base = Manifest::new();
        base.insert(path("a.txt"), meta(&[("h1", 10)], 0o644));

        // Same chunks: no change.
        assert!(changes(&base, &base).is_empty());

        let mut edited = base.clone();
        edited.insert(path("a.txt"), meta(&[("h1", 10), ("h2", 5)], 0o644));
        assert_eq!(
            changes(&base, &edited),
            vec![Change::Put {
                path: path("a.txt"),
                meta: meta(&[("h1", 10), ("h2", 5)], 0o644),
            }]
        );
    }

    #[test]
    fn an_mtime_only_change_is_not_a_change() {
        let mut base = Manifest::new();
        base.insert(path("a.txt"), meta(&[("h1", 10)], 0o644));

        let mut touched = base.clone();
        touched.get_mut(&path("a.txt")).unwrap().mtime_ns = 9_999;
        assert!(changes(&base, &touched).is_empty());
    }

    #[test]
    fn a_mode_change_is_a_change() {
        let mut base = Manifest::new();
        base.insert(path("run.sh"), meta(&[("h1", 10)], 0o644));

        let mut executable = base.clone();
        executable.get_mut(&path("run.sh")).unwrap().mode = 0o755;
        assert_eq!(changes(&base, &executable).len(), 1);
    }
}
