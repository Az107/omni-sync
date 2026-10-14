//! A thin wrapper over `notify` that forwards write events to the daemon's channel.
//!
//! Read-only access events (`IN_OPEN`, `IN_CLOSE_NOWRITE`, `IN_ACCESS`) must be
//! filtered out here: `scan::scan` opens directories and files on every push
//! check, so forwarding read events would make the watcher feed itself in a
//! tight loop and starve the pull stream in a biased `select!`.

use std::path::Path;

use notify::event::{AccessKind, AccessMode, EventKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc::{self, UnboundedReceiver};

/// Keeps the underlying watcher alive. Dropping it stops watching.
pub struct WatchHandle {
    _watcher: RecommendedWatcher,
}

/// Watch `root` recursively, returning a handle (keep it alive) and the stream
/// of mutation events.
pub fn watch(root: &Path) -> notify::Result<(WatchHandle, UnboundedReceiver<notify::Event>)> {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res
            && is_relevant(&event)
        {
            // A closed receiver means the daemon is shutting down; ignoring the
            // send error is correct there.
            let _ = tx.send(event);
        }
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    Ok((WatchHandle { _watcher: watcher }, rx))
}

fn is_relevant(event: &notify::Event) -> bool {
    let relevant_kind = matches!(
        event.kind,
        EventKind::Create(_)
            | EventKind::Modify(_)
            | EventKind::Remove(_)
            | EventKind::Any
            | EventKind::Access(AccessKind::Close(AccessMode::Write))
    );
    if !relevant_kind {
        return false;
    }
    if !event.paths.is_empty()
        && event.paths.iter().all(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(crate::scan::is_ignored)
        })
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, ModifyKind};

    #[test]
    fn read_only_access_events_are_filtered_out() {
        let open = notify::Event::new(EventKind::Access(AccessKind::Open(AccessMode::Any)));
        let close_ro = notify::Event::new(EventKind::Access(AccessKind::Close(AccessMode::Read)));
        let close_write =
            notify::Event::new(EventKind::Access(AccessKind::Close(AccessMode::Write)));
        let create = notify::Event::new(EventKind::Create(CreateKind::File));
        let modify = notify::Event::new(EventKind::Modify(ModifyKind::Any));

        assert!(!is_relevant(&open));
        assert!(!is_relevant(&close_ro));
        assert!(is_relevant(&close_write));
        assert!(is_relevant(&create));
        assert!(is_relevant(&modify));
    }
}
