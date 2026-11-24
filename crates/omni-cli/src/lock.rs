//! Exclusive ownership of one client config/database.
//!
//! The daemon and every one-shot CLI engine ([`crate::add`], `flush`,
//! `resolve`) take an OS advisory lock on `<stem>.agent.lock` *before* they
//! open the store or start an engine. The kernel releases the lock when the
//! process exits, so a crashed daemon never leaves a stale lock that needs
//! manual recovery.
//!
//! The lock file itself is deliberately never removed. Deleting it would let a
//! second process create and lock a fresh inode while the first still holds
//! the old one, which is exactly the concurrent-engine race this prevents.
//! A leftover file with no lock is harmless: the next process locks it.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

/// Why an instance lock could not be taken.
#[derive(Debug)]
pub enum LockError {
    /// Another live process owns this config/database.
    Busy,
    /// The lock file could not be created or inspected.
    Io(std::io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Busy => formatter.write_str("another process owns this client"),
            LockError::Io(error) => write!(formatter, "lock file: {error}"),
        }
    }
}

impl std::error::Error for LockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LockError::Busy => None,
            LockError::Io(error) => Some(error),
        }
    }
}

impl From<std::io::Error> for LockError {
    fn from(error: std::io::Error) -> Self {
        LockError::Io(error)
    }
}

/// A held lock. Dropping it (or exiting the process) releases the OS lock; the
/// file stays on disk so the inode identity never changes.
#[derive(Debug)]
pub struct InstanceLock {
    file: File,
}

impl InstanceLock {
    /// Take the exclusive lock for `config_path`, or report [`LockError::Busy`]
    /// immediately when another process holds it.
    pub fn try_acquire(config_path: &Path) -> Result<Self, LockError> {
        let path = lock_path(config_path);
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }

        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        if !try_lock(&file)? {
            return Err(LockError::Busy);
        }
        Ok(Self { file })
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Closing the descriptor releases the lock; unlock explicitly first so
        // a lock held via a shared open-file description is not inherited by a
        // child process that happened to keep the descriptor.
        let _ = unlock(&self.file);
    }
}

/// `<stem>.agent.lock` next to `<stem>.agent.db`, so two clients in one
/// directory (tests, the e2e script) never share ownership.
pub fn lock_path(config_path: &Path) -> PathBuf {
    let stem = config_path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("client");
    let dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!("{stem}.agent.lock"))
}

/// A user-facing explanation for a busy lock.
pub fn busy_message(config_path: &Path) -> String {
    format!(
        "another omni-sync process holds the lock for {} (a daemon may be running or still starting); \
         refusing to open a second engine on the same database",
        config_path.display()
    )
}

fn try_lock(file: &File) -> std::io::Result<bool> {
    // File locking is part of std on the pinned Rust 1.89 toolchain. Using it
    // avoids hand-written platform FFI and fails closed on unsupported targets.
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

fn unlock(file: &File) -> std::io::Result<()> {
    file.unlock()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_instance_is_refused_until_the_first_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("client.toml");

        let first = InstanceLock::try_acquire(&config).unwrap();
        assert!(lock_path(&config).exists());
        assert!(matches!(
            InstanceLock::try_acquire(&config),
            Err(LockError::Busy)
        ));

        drop(first);
        // The OS released the lock; the file itself remains (never removed).
        assert!(lock_path(&config).exists());
        let _second = InstanceLock::try_acquire(&config).unwrap();
    }

    #[test]
    fn locks_are_per_config_stem_not_per_directory() {
        let dir = tempfile::tempdir().unwrap();
        let _a = InstanceLock::try_acquire(&dir.path().join("a.toml")).unwrap();
        let _b = InstanceLock::try_acquire(&dir.path().join("b.toml")).unwrap();
        assert_ne!(
            lock_path(&dir.path().join("a.toml")),
            lock_path(&dir.path().join("b.toml"))
        );
    }
}
