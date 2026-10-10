//! SQLite storage for omni-sync.
//!
//! One connection behind a mutex, WAL, foreign keys on, busy timeout set. Every
//! async caller goes through [`Store::blocking`], a `spawn_blocking` hop, so a
//! query never stalls the tokio runtime — the same rule `imp-store` follows.
//!
//! Migrations are the forward-only list in [`migrations::MIGRATIONS`].

pub mod devices;
pub mod folders;
pub mod migrations;
pub mod pairing;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;

/// Everything that can go wrong talking to the database.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("blocking task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("store mutex poisoned")]
    Poisoned,
}

/// A handle to the database. Cloneable; all clones share one connection.
#[derive(Clone)]
pub struct Store {
    db: Arc<Mutex<Connection>>,
}

impl Store {
    /// Open (or create) a database file, then migrate it.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        tune(&conn, true)?;
        let store = Self {
            db: Arc::new(Mutex::new(conn)),
        };
        store.migrate()?;
        Ok(store)
    }

    /// An in-memory database, for tests.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory()?;
        tune(&conn, false)?;
        let store = Self {
            db: Arc::new(Mutex::new(conn)),
        };
        store.migrate()?;
        Ok(store)
    }

    /// Apply pending migrations.
    pub fn migrate(&self) -> Result<(), StoreError> {
        let mut conn = self.db.lock().map_err(|_| StoreError::Poisoned)?;
        migrations::apply(&mut conn)
    }

    /// Run a blocking database closure off the async runtime.
    pub async fn blocking<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        F: FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static,
        T: Send + 'static,
    {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || {
            let conn = db.lock().map_err(|_| StoreError::Poisoned)?;
            f(&conn)
        })
        .await?
    }

    /// Like [`Store::blocking`], but for a closure that needs `&mut Connection`
    /// (a transaction, such as consuming a pairing code).
    pub async fn blocking_mut<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        F: FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
        T: Send + 'static,
    {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || {
            let mut conn = db.lock().map_err(|_| StoreError::Poisoned)?;
            f(&mut conn)
        })
        .await?
    }

    /// Synchronous access for CLI paths that are already off the runtime.
    pub fn with<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        F: FnOnce(&Connection) -> Result<T, StoreError>,
    {
        let conn = self.db.lock().map_err(|_| StoreError::Poisoned)?;
        f(&conn)
    }

    /// Like [`Store::with`], but hands out a mutable connection for a
    /// transaction (e.g. consuming a pairing code atomically).
    pub fn with_mut<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        F: FnOnce(&mut Connection) -> Result<T, StoreError>,
    {
        let mut conn = self.db.lock().map_err(|_| StoreError::Poisoned)?;
        f(&mut conn)
    }
}

fn tune(conn: &Connection, file_backed: bool) -> Result<(), StoreError> {
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    if file_backed {
        // Returns "wal"; a failure here is not fatal for a fresh small database.
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, token_hash: u8) -> devices::Device {
        devices::Device {
            id: id.into(),
            name: "laptop".into(),
            pubkey: vec![],
            token_hash: vec![token_hash; 32],
            revoked: false,
        }
    }

    #[test]
    fn a_fresh_database_migrates_and_stores_a_device() {
        let store = Store::open_in_memory().unwrap();
        store
            .with(|conn| devices::insert(conn, &device("dev-1", 7)))
            .unwrap();
        let found = store
            .with(|conn| devices::by_token_hash(conn, &[7u8; 32]))
            .unwrap()
            .expect("device should be found by token hash");
        assert_eq!(found.id, "dev-1");
        assert!(!found.revoked);
        assert_eq!(store.with(devices::count).unwrap(), 1);
    }

    #[test]
    fn a_pairing_code_is_consumed_exactly_once() {
        let store = Store::open_in_memory().unwrap();
        store
            .with(|conn| pairing::create(conn, "ABCD-1234", 600))
            .unwrap();

        // First use succeeds, second does not, and an unknown code does not.
        let first = store
            .with_mut(|conn| pairing::consume(conn, "ABCD-1234", "dev-1"))
            .unwrap();
        let second = store
            .with_mut(|conn| pairing::consume(conn, "ABCD-1234", "dev-2"))
            .unwrap();
        let unknown = store
            .with_mut(|conn| pairing::consume(conn, "NOPE", "dev-3"))
            .unwrap();
        assert!(first, "the first consumption should succeed");
        assert!(!second, "a code must not be usable twice");
        assert!(!unknown, "an unknown code must be refused");
    }

    #[test]
    fn folders_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let folder = folders::Folder {
            id: "f1".into(),
            name: "Documents".into(),
            retention_versions: 50,
            retention_days: 90,
            encrypted: false,
        };
        store.with(|conn| folders::create(conn, &folder)).unwrap();
        let all = store.with(folders::list).unwrap();
        assert_eq!(all, vec![folder]);
    }

    #[tokio::test]
    async fn blocking_hops_off_the_runtime() {
        let store = Store::open_in_memory().unwrap();
        store
            .blocking(|conn| devices::insert(conn, &device("dev-9", 1)))
            .await
            .unwrap();
        let count = store.blocking(devices::count).await.unwrap();
        assert_eq!(count, 1);
    }
}
