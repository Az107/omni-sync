//! Why a file is not being synced right now.
//!
//! Detection is not implemented in M0. On Linux it will scan `/proc/*/fd` and
//! `/proc/*/maps` for the path (unprivileged, own uid) and match `/proc/<pid>/comm`
//! against the hold globs; other platforms fall back to `lsof`/`fuser` and then
//! to the quiet timeout. Until then, [`held`] reports nothing, which is the safe
//! answer: it never claims a hold it cannot prove.

use std::path::Path;

/// The reason a path is held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeldReason {
    /// A process has the file open or mapped (by pid).
    FileOpen { pid: u32 },
    /// A process matching a hold glob is running.
    ProcessRunning { name: String },
    /// Holder detection was unavailable; waiting out the quiet timeout.
    QuietPeriod,
}

/// Inspect the system for a hold on `path`.
///
/// M0 stub: always `None`. The `/proc` scan is M3 work.
pub fn held(_path: &Path) -> Option<HeldReason> {
    None
}
