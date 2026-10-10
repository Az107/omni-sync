//! Shared application state: the store, the config, and dashboard sessions.
//!
//! Sessions are an in-memory set of random handles. Restarting the server logs
//! everyone out, which is acceptable for a single-operator dashboard; durability
//! is not worth a second table yet.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use omni_store::Store;
use rand::Rng as _;

use crate::config::Config;

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub config: Arc<Config>,
    sessions: Arc<Mutex<HashSet<String>>>,
}

impl AppState {
    pub fn new(store: Store, config: Config) -> Self {
        Self {
            store,
            config: Arc::new(config),
            sessions: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Create a session and return its handle.
    pub fn create_session(&self) -> String {
        let mut bytes = [0u8; 32];
        rand::rng().fill(&mut bytes);
        let handle = hex::encode(bytes);
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.insert(handle.clone());
        }
        handle
    }

    pub fn session_valid(&self, handle: &str) -> bool {
        self.sessions
            .lock()
            .map(|sessions| sessions.contains(handle))
            .unwrap_or(false)
    }

    pub fn drop_session(&self, handle: &str) {
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.remove(handle);
        }
    }
}
