//! A thin wrapper over `notify` that forwards events to the daemon's channel.
//!
//! The debounce (500 ms of quiet before an op is emitted) and the self-write
//! guard belong to the loop that consumes these events, not here.

use std::path::Path;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc::UnboundedSender;

/// Keeps the underlying watcher alive. Dropping it stops watching.
pub struct WatchHandle {
    _watcher: RecommendedWatcher,
}

/// Watch `root` recursively, sending every raw event to `tx`.
pub fn watch(root: &Path, tx: UnboundedSender<notify::Event>) -> notify::Result<WatchHandle> {
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            // A closed receiver means the daemon is shutting down; ignoring the
            // send error is correct there.
            let _ = tx.send(event);
        }
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    Ok(WatchHandle { _watcher: watcher })
}
