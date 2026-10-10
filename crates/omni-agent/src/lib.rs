//! The omni-sync client daemon.
//!
//! M0 provides the pieces the daemon is built from — the sync-mode policy and a
//! real filesystem watcher — but not the sync loop itself. Watching, chunking,
//! uploading and applying land in M3, on top of these modules.

mod hold;
mod policy;
mod watcher;

pub use hold::{HeldReason, held};
pub use policy::{HoldRule, SyncMode};
pub use watcher::{WatchHandle, watch};

/// The default content-defined chunker configuration for this device.
pub fn default_chunker() -> omni_chunk::Chunker {
    omni_chunk::Chunker::new(omni_chunk::Config::default())
}
