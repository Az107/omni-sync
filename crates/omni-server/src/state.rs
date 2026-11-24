//! Shared application state: the store, the config, dashboard sessions, and the
//! small guards that keep password hashing and enrollment cheap to abuse.
//!
//! Sessions are an in-memory set of random handles with an absolute expiry.
//! Restarting the server logs everyone out, which is acceptable for a
//! single-operator dashboard; durability is not worth a second table yet.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use omni_core::{Op, OpKind};
use omni_store::sync::{self, AppliedOp};
use omni_store::{Store, StoreError};
use rand::Rng as _;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast};

use crate::chunkstore::ChunkStore;
use crate::config::Config;

/// How far a streaming subscriber may lag before it is told to resync. The
/// stream re-reads the folder head on lag, so this only bounds memory.
const OP_EVENT_CAPACITY: usize = 64;

/// Matches the cookie's `Max-Age`. An expired handle is rejected even if a
/// browser replays the old cookie.
const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// A hard ceiling on live dashboard sessions. Creating a session evicts the
/// oldest one, so an attacker cannot grow the map without bound.
const MAX_SESSIONS: usize = 1024;

/// Login/enrollment attempts allowed per client key in one window.
const AUTH_WINDOW: Duration = Duration::from_secs(60);
const AUTH_MAX_ATTEMPTS: u32 = 10;

/// Bound on how many distinct rate-limit keys are remembered. Reaching it
/// evicts the oldest window rather than allocating forever.
const MAX_AUTH_KEYS: usize = 4096;

/// Argon2 is deliberately expensive. Only this many password verifications may
/// run at once, and they always run on the blocking pool, so a login flood
/// cannot stall the async runtime or pin every CPU.
const MAX_CONCURRENT_HASHES: usize = 4;

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub config: Arc<Config>,
    pub chunks: ChunkStore,
    sessions: Arc<Mutex<Sessions>>,
    auth: Arc<AuthGate>,
    /// Fired with a folder id whenever that folder's op log grows. The SSE
    /// endpoint turns this into a wake-up, so clients pull on a change instead
    /// of on a timer.
    op_events: broadcast::Sender<String>,
}

struct Sessions {
    ttl: Duration,
    handles: HashMap<String, Instant>,
}

impl Sessions {
    fn prune(&mut self, now: Instant) {
        let ttl = self.ttl;
        self.handles
            .retain(|_, created| now.saturating_duration_since(*created) < ttl);
    }

    fn insert(&mut self, handle: String, now: Instant) {
        self.prune(now);
        while self.handles.len() >= MAX_SESSIONS {
            let Some(oldest) = self
                .handles
                .iter()
                .min_by_key(|(_, created)| **created)
                .map(|(handle, _)| handle.clone())
            else {
                break;
            };
            self.handles.remove(&oldest);
        }
        self.handles.insert(handle, now);
    }

    fn valid(&mut self, handle: &str, now: Instant) -> bool {
        self.prune(now);
        self.handles.contains_key(handle)
    }
}

struct AttemptWindow {
    started: Instant,
    count: u32,
}

struct AuthGate {
    attempts: Mutex<HashMap<String, AttemptWindow>>,
    hashing: Arc<Semaphore>,
}

impl AppState {
    pub fn new(store: Store, config: Config) -> Self {
        Self::with_session_ttl(store, config, DEFAULT_SESSION_TTL)
    }

    /// Build state with a specific session lifetime. The HTTP layer uses
    /// [`AppState::new`]; tests use this to observe expiry without waiting a day.
    pub fn with_session_ttl(store: Store, config: Config, session_ttl: Duration) -> Self {
        let chunks = ChunkStore::new(config.chunks_dir());
        let (op_events, _) = broadcast::channel(OP_EVENT_CAPACITY);
        Self {
            store,
            config: Arc::new(config),
            chunks,
            sessions: Arc::new(Mutex::new(Sessions {
                ttl: session_ttl,
                handles: HashMap::new(),
            })),
            auth: Arc::new(AuthGate {
                attempts: Mutex::new(HashMap::new()),
                hashing: Arc::new(Semaphore::new(MAX_CONCURRENT_HASHES)),
            }),
            op_events,
        }
    }

    /// Announce that `folder_id`'s op log grew. Called after ops are applied.
    pub fn publish_ops(&self, folder_id: &str) {
        // Nobody streaming is not an error, so a send with no subscribers is
        // deliberately ignored.
        let _ = self.op_events.send(folder_id.to_string());
    }

    /// Subscribe to op-log announcements. Subscribe *before* reading the head,
    /// so a change landing in between is delivered rather than missed.
    pub fn subscribe_ops(&self) -> broadcast::Receiver<String> {
        self.op_events.subscribe()
    }

    /// Apply one op on behalf of a device, off the async runtime.
    ///
    /// Used by the dashboard restore paths as well as the API, so it performs
    /// the same chunk-existence check: a version whose bytes are gone must not
    /// become active again.
    pub async fn apply(
        &self,
        folder_id: &str,
        device_id: &str,
        op: Op,
    ) -> Result<AppliedOp, StoreError> {
        self.validate_op_chunks(std::slice::from_ref(&op)).await?;
        let mut applied = self
            .apply_batch_inner(folder_id, device_id, vec![op])
            .await?;
        applied
            .pop()
            .ok_or_else(|| StoreError::InvalidOp("no op to apply".into()))
    }

    /// Apply a submitted batch atomically: either every op commits or none do.
    pub async fn apply_batch(
        &self,
        folder_id: &str,
        device_id: &str,
        ops: &[Op],
    ) -> Result<Vec<AppliedOp>, StoreError> {
        self.validate_op_chunks(ops).await?;
        self.apply_batch_inner(folder_id, device_id, ops.to_vec())
            .await
    }

    async fn apply_batch_inner(
        &self,
        folder_id: &str,
        device_id: &str,
        ops: Vec<Op>,
    ) -> Result<Vec<AppliedOp>, StoreError> {
        let folder_id = folder_id.to_string();
        let device_id = device_id.to_string();
        self.store
            .blocking_mut(move |conn| sync::apply_ops(conn, &folder_id, &device_id, &ops))
            .await
    }

    /// Verify that every chunk an op references is both recorded with the
    /// claimed length and actually present on disk with that length.
    ///
    /// The store repeats the row check inside the commit transaction; this
    /// layer adds the on-disk proof, which is the part the store cannot see.
    async fn validate_op_chunks(&self, ops: &[Op]) -> Result<(), StoreError> {
        for op in ops {
            sync::validate_op(op)?;
        }
        let mut expected: HashMap<String, u64> = HashMap::new();
        for op in ops.iter().filter(|op| op.kind != OpKind::Delete) {
            for chunk in &op.chunks {
                if let Some(previous) = expected.insert(chunk.hash.clone(), chunk.len)
                    && previous != chunk.len
                {
                    return Err(StoreError::InvalidOp(format!(
                        "chunk {} is claimed with two different lengths",
                        chunk.hash
                    )));
                }
            }
        }
        if expected.is_empty() {
            return Ok(());
        }

        let hashes: Vec<String> = expected.keys().cloned().collect();
        let rows = self
            .store
            .blocking(move |conn| sync::chunk_sizes(conn, &hashes))
            .await?;

        let chunks = self.chunks.clone();
        tokio::task::spawn_blocking(move || {
            for (hash, len) in &expected {
                match rows.get(hash) {
                    None => {
                        return Err(StoreError::MissingChunk { hash: hash.clone() });
                    }
                    Some(size) if size != len => {
                        return Err(StoreError::InvalidOp(format!(
                            "chunk {hash} is recorded with length {size}, op claims {len}"
                        )));
                    }
                    _ => {}
                }
                match chunks
                    .len(hash)
                    .map_err(|error| StoreError::Io(std::io::Error::other(error.to_string())))?
                {
                    None => {
                        return Err(StoreError::MissingChunk { hash: hash.clone() });
                    }
                    Some(size) if size != *len => {
                        return Err(StoreError::InvalidOp(format!(
                            "chunk {hash} is stored with length {size}, op claims {len}"
                        )));
                    }
                    _ => {}
                }
            }
            Ok(())
        })
        .await?
    }

    /// Create a session and return its handle.
    pub fn create_session(&self) -> String {
        let mut bytes = [0u8; 32];
        rand::rng().fill(&mut bytes);
        let handle = hex::encode(bytes);
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.insert(handle.clone(), Instant::now());
        }
        handle
    }

    pub fn session_valid(&self, handle: &str) -> bool {
        self.sessions
            .lock()
            .map(|mut sessions| sessions.valid(handle, Instant::now()))
            .unwrap_or(false)
    }

    pub fn drop_session(&self, handle: &str) {
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.handles.remove(handle);
        }
    }

    /// Number of live session handles. Exposed for tests and diagnostics; the
    /// set is always bounded by [`MAX_SESSIONS`].
    pub fn session_count(&self) -> usize {
        self.sessions
            .lock()
            .map(|sessions| sessions.handles.len())
            .unwrap_or(0)
    }

    /// Record one login/enrollment attempt for `key`. Returns `false` once the
    /// key has spent its window's budget.
    pub fn auth_allowed(&self, key: &str) -> bool {
        let Ok(mut attempts) = self.auth.attempts.lock() else {
            return false;
        };
        let now = Instant::now();
        attempts.retain(|_, window| now.saturating_duration_since(window.started) < AUTH_WINDOW);
        if attempts.len() >= MAX_AUTH_KEYS && !attempts.contains_key(key) {
            let oldest = attempts
                .iter()
                .min_by_key(|(_, window)| window.started)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                attempts.remove(&oldest);
            }
        }
        let window = attempts.entry(key.to_string()).or_insert(AttemptWindow {
            started: now,
            count: 0,
        });
        if now.saturating_duration_since(window.started) >= AUTH_WINDOW {
            *window = AttemptWindow {
                started: now,
                count: 0,
            };
        }
        if window.count >= AUTH_MAX_ATTEMPTS {
            return false;
        }
        window.count += 1;
        true
    }

    /// Forget a key's failed attempts after a successful authentication.
    pub fn auth_reset(&self, key: &str) {
        if let Ok(mut attempts) = self.auth.attempts.lock() {
            attempts.remove(key);
        }
    }

    /// A permit to run one Argon2 verification on the blocking pool. Saturation
    /// fails fast rather than queuing arbitrarily many password buffers.
    pub fn hash_permit(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.auth.hashing).try_acquire_owned().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_ttl(ttl: Duration) -> AppState {
        AppState::with_session_ttl(Store::open_in_memory().unwrap(), Config::default(), ttl)
    }

    #[test]
    fn password_hash_concurrency_is_bounded_without_waiting() {
        let state = state_with_ttl(Duration::from_secs(60));
        let permits: Vec<_> = (0..MAX_CONCURRENT_HASHES)
            .map(|_| state.hash_permit().unwrap())
            .collect();
        assert!(state.hash_permit().is_none());
        drop(permits);
        assert!(state.hash_permit().is_some());
    }

    #[tokio::test]
    async fn sessions_expire_after_their_ttl() {
        let state = state_with_ttl(Duration::from_millis(10));
        let handle = state.create_session();
        assert!(state.session_valid(&handle));
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(!state.session_valid(&handle), "an expired handle must fail");
    }

    #[test]
    fn the_session_set_is_bounded() {
        let state = state_with_ttl(Duration::from_secs(60));
        for _ in 0..(MAX_SESSIONS + 5) {
            state.create_session();
        }
        assert_eq!(state.session_count(), MAX_SESSIONS);
    }

    #[test]
    fn authentication_attempts_are_rate_limited_per_key() {
        let state = state_with_ttl(Duration::from_secs(60));
        for _ in 0..AUTH_MAX_ATTEMPTS {
            assert!(state.auth_allowed("client"));
        }
        assert!(!state.auth_allowed("client"), "the window is spent");
        // A different key has its own budget.
        assert!(state.auth_allowed("other"));
        // A successful login clears the key.
        state.auth_reset("client");
        assert!(state.auth_allowed("client"));
    }
}
