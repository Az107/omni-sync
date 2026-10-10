//! A thin wrapper over `notify` that forwards events to the daemon's channel.
//!
//! The engine owns the debounce and the self-write decision; this only turns
//! filesystem changes into a stream. Events our own applies cause do not need a
//! special case here: the next scan compares metadata against the last-synced
//! state and finds nothing to push.

use std::path::Path;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc::{self, UnboundedReceiver};

/// Keeps the underlying watcher alive. Dropping it stops watching.
pub struct WatchHandle {
    _watcher: RecommendedWatcher,
}

/// Watch `root` recursively, returning a handle (keep it alive) and the stream
/// of raw events.
pub fn watch(root: &Path) -> notify::Result<(WatchHandle, UnboundedReceiver<notify::Event>)> {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            // A closed receiver means the daemon is shutting down; ignoring the
            // send error is correct there.
            let _ = tx.send(event);
        }
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    Ok((WatchHandle { _watcher: watcher }, rx))
}
