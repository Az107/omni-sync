//! The sync engine.
//!
//! Push and pull are both **event-driven**, and neither spins on a timer:
//!
//! - **push** needs to walk and hash the tree, so the `notify` watcher fires it
//!   on a mutation event, with a slow safety scan (30 s) to catch anything the
//!   watcher missed (edits while the daemon was down).
//! - **pull** is woken by the server's op-log stream, so a remote change lands
//!   in round-trip time instead of on a polling interval. A slow fallback poll
//!   (60 s) covers a server that has no stream, or a connection that died
//!   without the socket noticing.
//!
//! The scan itself only reads files whose size/mtime/mode changed since the
//! last sync; unchanged files reuse their known chunk list without a read.
//!
//! # Engine Invariants (Do Not Break)
//!
//! 1. **Watcher events must be mutations only (`crate::watcher::is_relevant`).**
//!    `scan::scan` calls `fs::read_dir` and `fs::read` on every push check.
//!    On Linux `inotify`, forwarding read-only `EventKind::Access(Open | Read |
//!    Close(Read))` events causes `scan::scan` to enqueue a new watcher event
//!    while `push_inner` is running, locking the `select!` loop into a 250 ms
//!    self-scan loop and starving `wake.recv()` (the pull stream).
//! 2. **`sync_step` ordering depends on `applied_seq`.**
//!    - When `applied_seq == 0` (first sync of a folder on this device), run
//!      `pull_inner` *before* `push_inner` so pre-existing identical files on
//!      disk adopt the server's `version_id` instead of being pushed with
//!      `base_version_id = None` (which would create false overwrites). If a
//!      pre-existing local file on first sync has *different* content from the
//!      server's op, `pull_inner` leaves the local file on disk and records a
//!      synthetic baseline (`mtime_ns = 0`, `version_id = 0`) so `push_inner`
//!      uploads the local file and records an overwrite on the server.
//!    - When `applied_seq > 0` (already initialized), run `push_inner` *before*
//!      `pull_inner` so offline local edits are pushed with their known
//!      `base_version_id` before any incoming remote op can overwrite them on
//!      disk.
//! 3. **Global `ops.id` sequence & post-push pull reconciliation.**
//!    `ops.id` is `AUTOINCREMENT` across *all* folders on the server, so ops in
//!    another folder create sequence gaps in this folder. `advance_watermark`
//!    only advances across contiguous sequence numbers starting at `previous`
//!    (in case a foreign op in *this* folder landed inside the gap). Therefore:
//!    - `push_inner` calls `pull_inner` immediately after submitting a batch so
//!      cross-folder gaps are closed and the server's `members.acked_seq` is
//!      updated;
//!    - `pull_inner` skips `fetch_chunks`/`write_file` when
//!      `versions.get(&path) == Some(&entry.version_id)` and the file exists on
//!      disk, avoiding re-downloading and rewriting the op we just pushed.
//! 4. **Hold deferral in both directions (`SyncMode::OnClose` / `HoldRule`).**
//!    Held files are neither uploaded in `push_inner` nor rename-replaced on
//!    disk in `pull_inner` (replacing an open file orphans the holder's inode).
//!    While any hold is active (`has_holds`), `run_with_control` enables the
//!    500 ms `hold_retry` timer; when no holds exist, the timer is disabled.
//! 5. **SSE waker termination (`spawn_op_waker`).**
//!    Every await point in `spawn_op_waker` must select on `wake.closed()` so
//!    stopping or reconfiguring a folder in the daemon does not leak an
//!    infinite background SSE reconnect loop.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use omni_chunk::Chunker;
use omni_core::{Change, FileMeta, Ignore, Manifest, Op, OpKind, PathBytes, changes};
use omni_netproto::{OpDto, OpKindDto, decode_chunks, decode_path};
use omni_store::{Store, agent};
use omni_transfer::Client;
use tokio::sync::{mpsc, oneshot};

use crate::hold::{self, MAX_FD_SCAN_FILES};
use crate::policy::{HoldRule, SyncMode};
use crate::{apply, scan};

/// The safety net under the op stream: if the server has no event stream, or a
/// connection dies without the socket noticing, this still pulls eventually.
const FALLBACK_PULL_INTERVAL: Duration = Duration::from_secs(60);
/// The slow full-tree safety scan, for changes the watcher did not see.
const SAFETY_SCAN_INTERVAL: Duration = Duration::from_secs(30);
/// Quiet period after a filesystem event before scanning (coalesces save bursts).
const DEBOUNCE: Duration = Duration::from_millis(250);
/// Poll interval used only while a file or process hold is active, so releasing
/// an fd or exiting a held process is noticed within ~0.5 s.
const HOLD_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// One folder to keep in sync.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineConfig {
    pub folder_id: String,
    pub root: PathBuf,
    pub mode: SyncMode,
    pub hold_rule: HoldRule,
    /// Folder-level excludes shared with every other client. Empty means only
    /// the built-in droppings in [`crate::scan::is_ignored`].
    pub exclude: Vec<String>,
    /// When false, report `mode = 0` and never chmod on apply. Required when a
    /// client that cannot express POSIX modes shares the folder.
    pub preserve_permissions: bool,
}

impl EngineConfig {
    pub fn new(folder_id: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        Self {
            folder_id: folder_id.into(),
            root: root.into(),
            mode: SyncMode::Live,
            hold_rule: HoldRule::default(),
            exclude: Vec::new(),
            preserve_permissions: true,
        }
    }

    pub fn with_mode(mut self, mode: SyncMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_hold_rule(mut self, hold_rule: HoldRule) -> Self {
        self.hold_rule = hold_rule;
        self
    }

    pub fn with_exclude(mut self, exclude: Vec<String>) -> Self {
        self.exclude = exclude;
        self
    }

    pub fn with_preserve_permissions(mut self, preserve_permissions: bool) -> Self {
        self.preserve_permissions = preserve_permissions;
        self
    }
}

/// A request from the CLI control channel to force an immediate sync cycle on
/// a running engine.
pub struct FlushRequest {
    /// When `Some`, only this relative path's hold is overridden; when `None`,
    /// every hold in the folder is overridden.
    pub path: Option<PathBytes>,
    /// True for `flush` (override holds), false for an ordinary immediate sync.
    pub override_holds: bool,
    pub reply: oneshot::Sender<Result<()>>,
}

/// Drives one folder against one server.
pub struct Engine {
    client: Client,
    store: Store,
    chunker: Chunker,
    ignore: Ignore,
    config: EngineConfig,
}

/// An op plus what to record locally once the server accepts it.
struct Pending {
    dto: OpDto,
    path: PathBytes,
    meta: Option<FileMeta>,
}

impl Engine {
    pub fn new(client: Client, store: Store, config: EngineConfig) -> Self {
        let ignore = Ignore::new(&config.exclude);
        Self {
            client,
            store,
            chunker: Chunker::new(omni_chunk::Config::default()),
            ignore,
            config,
        }
    }

    /// Run until the watcher's channel closes, logging and surviving cycle
    /// failures.
    pub async fn run(&self) -> Result<()> {
        self.run_with_control(None).await
    }

    /// Run the engine loop with an optional control receiver for on-demand
    /// `flush` / sync requests from the CLI.
    pub async fn run_with_control(
        &self,
        mut control: Option<mpsc::Receiver<FlushRequest>>,
    ) -> Result<()> {
        let (_handle, mut events) = crate::watcher::watch(&self.config.root)
            .with_context(|| format!("watching {}", self.config.root.display()))?;

        // The stream is the fast path; the timer is the safety net.
        let mut wake = spawn_op_waker(self.client.clone(), self.config.folder_id.clone());
        let mut fallback = tokio::time::interval(FALLBACK_PULL_INTERVAL);
        let mut safety = tokio::time::interval(SAFETY_SCAN_INTERVAL);
        let mut hold_retry = tokio::time::interval(HOLD_RETRY_INTERVAL);
        fallback.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        safety.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        hold_retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Consume the immediate first ticks because `sync_once` below performs
        // the initial reconciliation right away.
        fallback.tick().await;
        safety.tick().await;
        hold_retry.tick().await;

        let mut has_holds = self
            .logged_cycle("initial sync", self.sync_step(false, None))
            .await;

        loop {
            tokio::select! {
                biased;
                Some(request) = recv_control(&mut control) => {
                    let result = if request.override_holds {
                        self.flush_once(request.path.as_deref()).await
                    } else {
                        self.sync_once().await
                    };
                    has_holds = self.has_recorded_holds().await;
                    let _ = request.reply.send(result);
                }
                Some(()) = wake.recv() => {
                    has_holds = self.logged_cycle("pull", self.sync_step(false, None)).await;
                }
                event = events.recv() => {
                    if event.is_none() {
                        return Ok(());
                    }
                    // Let a burst of writes settle, then drain the rest.
                    tokio::time::sleep(DEBOUNCE).await;
                    while events.try_recv().is_ok() {}
                    has_holds = self.logged_cycle("push", self.push_inner(false, None)).await;
                }
                _ = hold_retry.tick(), if has_holds => {
                    has_holds = self.logged_cycle("hold retry", self.sync_step(false, None)).await;
                }
                _ = safety.tick() => {
                    has_holds = self.logged_cycle("safety scan", self.push_inner(false, None)).await;
                }
                _ = fallback.tick() => {
                    has_holds = self.logged_cycle("pull", self.sync_step(false, None)).await;
                }
            }
        }
    }

    async fn logged_cycle(
        &self,
        what: &str,
        work: impl std::future::Future<Output = Result<bool>>,
    ) -> bool {
        match work.await {
            Ok(held) => held,
            Err(error) => {
                tracing::warn!(folder = %self.config.folder_id, %error, "{} failed", what);
                false
            }
        }
    }

    async fn has_recorded_holds(&self) -> bool {
        let folder = self.config.folder_id.clone();
        self.store
            .blocking(move |conn| Ok(!agent::held_files(conn, &folder)?.is_empty()))
            .await
            .unwrap_or(false)
    }

    /// One full sync cycle, respecting the folder's sync mode and hold rules.
    ///
    /// On a first sync (`applied_seq == 0`), we pull before pushing so we see
    /// what the server already has instead of reporting existing files as
    /// overwrites. Once initialized (`applied_seq > 0`), we push local offline
    /// edits first so their `base_version_id` is sequenced before incoming
    /// remote edits.
    pub async fn sync_once(&self) -> Result<()> {
        self.sync_step(false, None).await?;
        Ok(())
    }

    /// Override holds (for `target_path` or the whole folder) and run one full
    /// push+pull cycle now.
    pub async fn flush_once(&self, target_path: Option<&[u8]>) -> Result<()> {
        let folder = self.config.folder_id.clone();
        let owned_target = target_path.map(<[u8]>::to_vec);
        self.store
            .blocking(move |conn| match owned_target {
                Some(path) => agent::set_held(conn, &folder, &path, None),
                None => agent::clear_held_folder(conn, &folder),
            })
            .await?;

        self.sync_step(true, target_path).await?;
        Ok(())
    }

    async fn sync_step(
        &self,
        ignore_holds: bool,
        only_override_path: Option<&[u8]>,
    ) -> Result<bool> {
        let folder = self.config.folder_id.clone();
        let applied_seq = self
            .store
            .blocking(move |conn| agent::applied_seq(conn, &folder))
            .await?;

        if applied_seq == 0 {
            let pull_held = self.pull_inner(ignore_holds, only_override_path).await?;
            let push_held = self.push_inner(ignore_holds, only_override_path).await?;
            Ok(pull_held || push_held)
        } else {
            let push_held = self.push_inner(ignore_holds, only_override_path).await?;
            let pull_held = self.pull_inner(ignore_holds, only_override_path).await?;
            Ok(push_held || pull_held)
        }
    }

    async fn push_inner(
        &self,
        ignore_holds: bool,
        only_override_path: Option<&[u8]>,
    ) -> Result<bool> {
        let folder = self.config.folder_id.clone();
        let store = self.store.clone();

        // What we last synced, each path's version, and our watermark.
        let (last, versions, applied_seq, previously_held) = {
            let (f1, f2, f3, f4) = (
                folder.clone(),
                folder.clone(),
                folder.clone(),
                folder.clone(),
            );
            let last = store
                .blocking(move |conn| agent::manifest(conn, &f1))
                .await?;
            let versions = store
                .blocking(move |conn| agent::versions(conn, &f2))
                .await?;
            let seq = store
                .blocking(move |conn| agent::applied_seq(conn, &f3))
                .await?;
            let held = store
                .blocking(move |conn| agent::held_files(conn, &f4))
                .await?;
            (last, versions, seq, held)
        };

        // A newly excluded path must leave the *baseline* as well as the scan.
        // `changes` treats a path in `last` but not in `current` as a Delete,
        // so leaving it here would delete the file on every other device the
        // moment a folder's excludes change.
        let mut last = baseline_excluding(last, &self.ignore);
        let previously_held: std::collections::BTreeMap<PathBytes, String> = previously_held
            .into_iter()
            .filter(|(path, _)| !self.ignore.matches(path))
            .collect();

        // Scan, reusing the last-synced chunks for files whose metadata is
        // unchanged (so an idle folder does no disk reads at all).
        let (root, chunker, previous) = (self.config.root.clone(), self.chunker, last.clone());
        let options = scan::ScanOptions {
            ignore: self.ignore.clone(),
            preserve_permissions: self.config.preserve_permissions,
        };
        let scanned = tokio::task::spawn_blocking(move || {
            scan::scan_with_deferred(&root, &chunker, &previous, options)
        })
        .await
        .context("scan task panicked")??;
        // A skipped unstable/unreadable/unsupported path is not a deletion.
        last.retain(|path, _| !scanned.is_deferred(path));
        let current = scanned.manifest;

        let local = local_changes(&last, &current, &versions);
        let dirty_paths: BTreeSet<PathBytes> = local
            .iter()
            .map(|change| match change {
                Change::Put { path, .. } | Change::Delete { path } => path.clone(),
            })
            .collect();

        // Clear stale holds for paths that are no longer dirty.
        for old_path in previously_held.keys() {
            if !dirty_paths.contains(old_path) {
                let folder = self.config.folder_id.clone();
                let path = old_path.clone();
                self.store
                    .blocking(move |conn| agent::set_held(conn, &folder, &path, None))
                    .await?;
            }
        }

        if local.is_empty() {
            return Ok(false);
        }

        let process_hold = hold::check_processes(&self.config.hold_rule);
        let skip_fd_scan = local.len() > MAX_FD_SCAN_FILES;
        let mut any_held = false;
        let mut pending = Vec::new();

        for change in &local {
            let path = match change {
                Change::Put { path, .. } | Change::Delete { path } => path,
            };
            let override_this =
                ignore_holds && only_override_path.is_none_or(|target| target == path.as_slice());

            if !override_this {
                let hold_reason = if let Some(reason) = &process_hold {
                    Some(reason.clone())
                } else if self.config.mode == SyncMode::OnClose {
                    match change {
                        Change::Put { path, .. } => {
                            let full = self.config.root.join(scan::path_from_bytes(path));
                            hold::check_file(&full, &self.config.hold_rule, skip_fd_scan)
                        }
                        Change::Delete { .. } => None,
                    }
                } else {
                    None
                };

                if let Some(reason) = hold_reason {
                    any_held = true;
                    let folder = self.config.folder_id.clone();
                    let path = path.clone();
                    let code = reason.code();
                    self.store
                        .blocking(move |conn| agent::set_held(conn, &folder, &path, Some(&code)))
                        .await?;
                    continue;
                }
            }

            match change {
                Change::Put { path, .. } => {
                    let (root, rel) = (self.config.root.clone(), path.clone());
                    let chunker = self.chunker;
                    let preserve_permissions = self.config.preserve_permissions;
                    let read = tokio::task::spawn_blocking(move || {
                        scan::chunk_file(&root, &rel, &chunker, preserve_permissions)
                    })
                    .await
                    .context("chunk task panicked")?;
                    let (meta, hashed) = match read {
                        Ok(read) => read,
                        Err(error) => {
                            tracing::warn!(path = ?path, %error, "deferring changed or unreadable file");
                            continue;
                        }
                    };
                    for chunk in &hashed {
                        self.client
                            .put_chunk(&chunk.hash, chunk.data.clone())
                            .await
                            .with_context(|| format!("uploading chunk {}", chunk.hash))?;
                    }
                    let op = self.op(OpKind::Put, path, &meta, &versions, applied_seq);
                    pending.push(Pending {
                        dto: OpDto::from_core(&op),
                        path: path.clone(),
                        meta: Some(meta),
                    });
                }
                Change::Delete { path } => {
                    let meta = FileMeta {
                        size: 0,
                        mtime_ns: 0,
                        mode: 0,
                        chunks: Vec::new(),
                    };
                    let op = self.op(OpKind::Delete, path, &meta, &versions, applied_seq);
                    pending.push(Pending {
                        dto: OpDto::from_core(&op),
                        path: path.clone(),
                        meta: None,
                    });
                }
            }
        }

        if pending.is_empty() {
            return Ok(any_held);
        }

        let pushed = pending.len();
        let dtos = pending.iter().map(|item| item.dto.clone()).collect();
        let response = self
            .client
            .submit_ops(&self.config.folder_id, dtos)
            .await
            .context("submitting ops")?;
        if response.applied.len() != pending.len() {
            return Err(anyhow!(
                "server returned an incomplete operation acknowledgement"
            ));
        }

        let store = self.store.clone();
        let folder = self.config.folder_id.clone();
        // Advance the watermark only over ops we really applied. The op ids are a
        // single global sequence, so another device's op can land *inside* our
        // batch; jumping the watermark to our last seq would skip it forever.
        let applied: Vec<i64> = response.applied.iter().map(|entry| entry.seq).collect();
        let watermark = advance_watermark(applied_seq, &applied);
        store
            .blocking_mut(move |conn| {
                for (item, applied) in pending.iter().zip(response.applied.iter()) {
                    match &item.meta {
                        Some(meta) => {
                            agent::set_file(conn, &folder, &item.path, meta, applied.version_id)?
                        }
                        None => agent::remove_file(conn, &folder, &item.path)?,
                    }
                }
                agent::set_applied_seq(conn, &folder, watermark)?;
                Ok(())
            })
            .await?;

        tracing::info!(folder = %self.config.folder_id, pushed, "pushed changes");

        // Reconcile the op log immediately so cross-folder seq gaps and the
        // server's `members.acked_seq` rail stay current after our push.
        let pull_held = self.pull_inner(ignore_holds, only_override_path).await?;
        Ok(any_held || pull_held)
    }

    fn op(
        &self,
        kind: OpKind,
        path: &PathBytes,
        meta: &FileMeta,
        versions: &std::collections::BTreeMap<PathBytes, i64>,
        base_seq: i64,
    ) -> Op {
        Op {
            base_seq,
            // A version id of 0 means "never synced"; send None so the server
            // reads it as a create, not an overwrite of version 0.
            base_version_id: versions.get(path).copied().filter(|v| *v > 0),
            kind,
            path: path.clone(),
            from: None,
            size: meta.size,
            mtime_ns: meta.mtime_ns,
            mode: meta.mode,
            chunks: meta.chunks.clone(),
            restored_from: None,
        }
    }

    async fn pull_inner(
        &self,
        ignore_holds: bool,
        only_override_path: Option<&[u8]>,
    ) -> Result<bool> {
        if !ignore_holds && hold::check_processes(&self.config.hold_rule).is_some() {
            return Ok(true);
        }

        let folder = self.config.folder_id.clone();
        let mut since = self
            .store
            .blocking(move |conn| agent::applied_seq(conn, &folder))
            .await?;
        let mut latest = std::collections::BTreeMap::new();
        loop {
            let response = self
                .client
                .ops_page(&self.config.folder_id, since, Some(500))
                .await?;
            let mut previous = since;
            for entry in &response.entries {
                if entry.seq <= previous {
                    return Err(anyhow!("server sent an unordered operation page"));
                }
                previous = entry.seq;
                let path =
                    decode_path(&entry.path).map_err(|_| anyhow!("server sent a bad path"))?;
                latest.insert(path, (entry.version_id, entry.kind));
            }
            let held = self
                .apply_entries(response.entries, ignore_holds, only_override_path)
                .await?;
            if held {
                return Ok(true);
            }
            if !response.has_more {
                self.adopt_equivalent_conflicts(&latest).await?;
                return Ok(false);
            }
            if previous <= since {
                return Err(anyhow!("server sent an operation page without progress"));
            }
            // Finish all historical pages before the first push. Bounded
            // per-request pages must not turn a partial bootstrap into a push
            // with unknown baselines for paths on later pages.
            since = previous;
        }
    }

    async fn apply_entries(
        &self,
        entries: Vec<omni_netproto::LogEntryDto>,
        ignore_holds: bool,
        only_override_path: Option<&[u8]>,
    ) -> Result<bool> {
        let folder = self.config.folder_id.clone();
        let (mut versions, mut baseline) = {
            let (f2, f3) = (folder.clone(), folder.clone());
            let versions = self
                .store
                .blocking(move |conn| agent::versions(conn, &f2))
                .await?;
            let baseline = self
                .store
                .blocking(move |conn| agent::manifest(conn, &f3))
                .await?;
            (versions, baseline)
        };

        for entry in entries {
            let path = decode_path(&entry.path).map_err(|_| anyhow!("server sent a bad path"))?;
            apply::validate_rel(&path).map_err(|_| anyhow!("server sent an unsafe path"))?;

            // An excluded path is never applied, but the watermark must still
            // move past it or every pull would replay it forever. This also
            // protects against a legacy client that pushed an excluded path
            // before the folder policy changed.
            if self.ignore.matches(&path) {
                let folder = folder.clone();
                let seq = entry.seq;
                self.store
                    .blocking(move |conn| agent::set_applied_seq(conn, &folder, seq))
                    .await?;
                continue;
            }

            if versions.get(&path) == Some(&0) {
                let folder = folder.clone();
                let seq = entry.seq;
                let remote = FileMeta {
                    size: if entry.kind == OpKindDto::Delete {
                        0
                    } else {
                        entry.size
                    },
                    mtime_ns: 0,
                    mode: if self.config.preserve_permissions {
                        entry.mode
                    } else {
                        0
                    },
                    chunks: if entry.kind == OpKindDto::Delete {
                        vec![]
                    } else {
                        decode_chunks(&entry.chunks)
                    },
                };
                baseline.insert(path.clone(), remote.clone());
                self.store
                    .blocking(move |conn| {
                        agent::set_file(conn, &folder, &path, &remote, 0)?;
                        agent::set_applied_seq(conn, &folder, seq)
                    })
                    .await?;
                continue;
            }
            let local_before = self.local_snapshot(&path)?;
            let disk_path = self.config.root.join(scan::path_from_bytes(&path));
            let disk_exists = local_before.is_some();
            let override_this =
                ignore_holds && only_override_path.is_none_or(|target| target == path.as_slice());

            if !override_this
                && self.config.mode == SyncMode::OnClose
                && disk_exists
                && let Some(reason) = hold::check_file(&disk_path, &self.config.hold_rule, false)
            {
                let folder = folder.clone();
                let code = reason.code();
                self.store
                    .blocking(move |conn| agent::set_held(conn, &folder, &path, Some(&code)))
                    .await?;
                return Ok(true);
            }

            // On first sync, a differing pre-existing file gets a synthetic
            // version-zero baseline. Keep it protected for the entire replay,
            // including later puts and deletes in the same historical tail.
            if !baseline.contains_key(&path)
                && !versions.contains_key(&path)
                && let Some(local) = local_before.as_ref()
                && matches!(
                    entry.kind,
                    OpKindDto::Put | OpKindDto::Resolve | OpKindDto::Move
                )
            {
                let remote = FileMeta {
                    size: entry.size,
                    mtime_ns: entry.mtime_ns,
                    mode: entry.mode,
                    chunks: decode_chunks(&entry.chunks),
                };
                if local.equivalent(&remote) {
                    let folder = folder.clone();
                    let seq = entry.seq;
                    let version = entry.version_id;
                    let adopted = local.clone();
                    versions.insert(path.clone(), version);
                    baseline.insert(path.clone(), local.clone());
                    self.store
                        .blocking(move |conn| {
                            agent::set_file(conn, &folder, &path, &adopted, version)?;
                            agent::set_applied_seq(conn, &folder, seq)
                        })
                        .await?;
                    continue;
                }
            }
            if pull_conflicts(baseline.get(&path), local_before.as_ref()) {
                let folder = folder.clone();
                let seq = entry.seq;
                if let Some(local) = local_before {
                    let remote_base = match entry.kind {
                        OpKindDto::Put | OpKindDto::Resolve | OpKindDto::Move => FileMeta {
                            size: entry.size,
                            mtime_ns: 0,
                            mode: entry.mode,
                            chunks: decode_chunks(&entry.chunks),
                        },
                        OpKindDto::Delete => FileMeta {
                            size: 0,
                            mtime_ns: 0,
                            mode: local.mode,
                            chunks: Vec::new(),
                        },
                    };
                    let version = 0;
                    versions.insert(path.clone(), version);
                    baseline.insert(path.clone(), remote_base.clone());
                    self.store
                        .blocking(move |conn| {
                            agent::set_file(conn, &folder, &path, &remote_base, version)?;
                            agent::set_applied_seq(conn, &folder, seq)
                        })
                        .await?;
                } else {
                    self.store
                        .blocking(move |conn| agent::set_applied_seq(conn, &folder, seq))
                        .await?;
                }
                continue;
            }
            let snapshot = local_before;

            match entry.kind {
                OpKindDto::Delete => {
                    apply::remove_file(&self.config.root, &path)
                        .with_context(|| format!("deleting {}", String::from_utf8_lossy(&path)))?;
                    versions.remove(&path);
                    baseline.remove(&path);
                    let folder = folder.clone();
                    self.store
                        .blocking(move |conn| agent::remove_file(conn, &folder, &path))
                        .await?;
                }
                OpKindDto::Put | OpKindDto::Resolve | OpKindDto::Move => {
                    let remote_chunks = decode_chunks(&entry.chunks);
                    // Skip re-downloading and rewriting our own already-applied
                    // version when a cross-folder sequence gap causes `ops_since`
                    // to replay it.
                    if versions.get(&path).copied() == Some(entry.version_id) && disk_exists {
                        let folder = folder.clone();
                        let seq = entry.seq;
                        self.store
                            .blocking(move |conn| agent::set_applied_seq(conn, &folder, seq))
                            .await?;
                        continue;
                    }

                    let bytes = self.fetch_chunks(&entry.chunks).await?;
                    if bytes.len() as u64 != entry.size {
                        return Err(anyhow!("server sent an inconsistent file size"));
                    }
                    // The file can become dirty while chunks are in flight.
                    // Re-check immediately before replacement; preserve the
                    // local edit and let push record it as an overwrite.
                    let local_after = self.local_snapshot(&path)?;
                    if changed_since_snapshot(snapshot.as_ref(), local_after.as_ref()) {
                        let folder = folder.clone();
                        let seq = entry.seq;
                        let baseline = FileMeta {
                            size: entry.size,
                            mtime_ns: 0,
                            mode: entry.mode,
                            chunks: remote_chunks.clone(),
                        };
                        versions.insert(path.clone(), 0);
                        self.store
                            .blocking(move |conn| {
                                agent::set_file(conn, &folder, &path, &baseline, 0)?;
                                agent::set_applied_seq(conn, &folder, seq)
                            })
                            .await?;
                        continue;
                    }
                    let applied_mode = if self.config.preserve_permissions {
                        entry.mode
                    } else {
                        0
                    };
                    apply::write_file(
                        &self.config.root,
                        &path,
                        &bytes,
                        &FileMeta {
                            size: entry.size,
                            mtime_ns: entry.mtime_ns,
                            mode: applied_mode,
                            chunks: remote_chunks.clone(),
                        },
                    )
                    .with_context(|| format!("writing {}", String::from_utf8_lossy(&path)))?;

                    // Record the *local* mtime, so the next scan's metadata fast
                    // path matches and does not re-read the file we just wrote.
                    let mtime_ns =
                        apply::local_mtime_ns(&self.config.root, &path).unwrap_or(entry.mtime_ns);
                    let meta = FileMeta {
                        size: entry.size,
                        mtime_ns,
                        mode: applied_mode,
                        chunks: remote_chunks,
                    };
                    let folder = folder.clone();
                    let version_id = entry.version_id;
                    versions.insert(path.clone(), version_id);
                    baseline.insert(path.clone(), meta.clone());
                    self.store
                        .blocking(move |conn| {
                            agent::set_file(conn, &folder, &path, &meta, version_id)
                        })
                        .await?;
                }
            }

            let folder = folder.clone();
            let seq = entry.seq;
            self.store
                .blocking(move |conn| agent::set_applied_seq(conn, &folder, seq))
                .await?;
        }
        Ok(false)
    }

    /// Fetch and verify every chunk of a file, in order.
    async fn fetch_chunks(&self, chunks: &[omni_netproto::ChunkRefDto]) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        for chunk in chunks {
            let data = self
                .client
                .get_chunk(&chunk.hash)
                .await?
                .ok_or_else(|| anyhow!("server is missing chunk {}", chunk.hash))?;
            if omni_chunk::hash_hex(&data) != chunk.hash {
                return Err(anyhow!("chunk {} failed verification", chunk.hash));
            }
            if data.len() as u64 != chunk.len {
                return Err(anyhow!("chunk {} has an inconsistent length", chunk.hash));
            }
            bytes.extend_from_slice(&data);
        }
        Ok(bytes)
    }

    fn local_snapshot(&self, path: &[u8]) -> Result<Option<FileMeta>> {
        match scan::chunk_file(
            &self.config.root,
            path,
            &self.chunker,
            self.config.preserve_permissions,
        ) {
            Ok((meta, _)) => Ok(Some(meta)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error)
                .with_context(|| format!("reading local file {path:?} before remote apply")),
        }
    }

    async fn adopt_equivalent_conflicts(
        &self,
        latest: &std::collections::BTreeMap<PathBytes, (i64, OpKindDto)>,
    ) -> Result<()> {
        let folder = self.config.folder_id.clone();
        let lookup = folder.clone();
        let (versions, baseline) = self
            .store
            .blocking(move |conn| {
                Ok((
                    agent::versions(conn, &lookup)?,
                    agent::manifest(conn, &lookup)?,
                ))
            })
            .await?;
        for (path, (version, kind)) in latest {
            if versions.get(path) != Some(&0) || self.ignore.matches(path) {
                continue;
            }
            let local = match self.local_snapshot(path) {
                Ok(local) => local,
                Err(error) => {
                    tracing::warn!(path = ?path, %error, "leaving local conflict pending");
                    continue;
                }
            };
            let folder = folder.clone();
            let path = path.clone();
            if *kind == OpKindDto::Delete {
                if local.is_none() {
                    self.store
                        .blocking(move |conn| agent::remove_file(conn, &folder, &path))
                        .await?;
                }
            } else if let (Some(local), Some(remote)) = (local, baseline.get(&path))
                && local.equivalent(remote)
            {
                let version = *version;
                self.store
                    .blocking(move |conn| agent::set_file(conn, &folder, &path, &local, version))
                    .await?;
            }
        }
        Ok(())
    }
}

async fn recv_control(control: &mut Option<mpsc::Receiver<FlushRequest>>) -> Option<FlushRequest> {
    match control {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// The push baseline with excluded paths removed.
///
/// This is the fix for the delete trap: `changes` turns "in `last`, not in the
/// scan" into a `Delete`. The scan already skips excluded paths, so the
/// baseline must skip them too, or a folder that gains an exclude would delete
/// every matching file on every other device.
fn baseline_excluding(last: Manifest, ignore: &Ignore) -> Manifest {
    last.into_iter()
        .filter(|(path, _)| !ignore.matches(path))
        .collect()
}

fn local_changes(
    last: &Manifest,
    current: &Manifest,
    versions: &std::collections::BTreeMap<PathBytes, i64>,
) -> Vec<Change> {
    let mut result = changes(last, current);
    let changed: BTreeSet<_> = result
        .iter()
        .map(|change| match change {
            Change::Put { path, .. } | Change::Delete { path } => path.clone(),
        })
        .collect();
    // Version zero is an explicit pending conflict, not an accepted baseline.
    // Even if the user changes it back to equivalent content, push once so the
    // synthetic marker cannot permanently block subsequent remote operations.
    for (path, version) in versions {
        if *version == 0 && last.contains_key(path) && !changed.contains(path) {
            result.push(match current.get(path) {
                Some(meta) => Change::Put {
                    path: path.clone(),
                    meta: meta.clone(),
                },
                None => Change::Delete { path: path.clone() },
            });
        }
    }
    result
}

/// A pull may replace a path only when the local file still matches what this
/// client last accepted. Missing/untracked local files are dirty as well.
fn pull_conflicts(baseline: Option<&FileMeta>, local: Option<&FileMeta>) -> bool {
    match (baseline, local) {
        (None, None) => false,
        (Some(base), Some(current)) => !base.equivalent(current),
        _ => true,
    }
}

fn changed_since_snapshot(before: Option<&FileMeta>, after: Option<&FileMeta>) -> bool {
    match (before, after) {
        (Some(before), Some(after)) => !before.equivalent(after),
        (None, None) => false,
        _ => true,
    }
}

/// Turn the server's op-log stream into wake-ups for the pull loop.
///
/// Running it as its own task keeps `run` a plain `select!`. It reconnects
/// forever on a capped backoff until the receiver is dropped (when the engine
/// stops), at which point `wake.closed()` terminates the task immediately.
fn spawn_op_waker(client: Client, folder_id: String) -> mpsc::Receiver<()> {
    // Capacity 1: a burst of remote changes only ever means "there is work".
    let (wake, rx) = mpsc::channel(1);
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        let mut degraded = false;
        loop {
            if wake.is_closed() {
                return;
            }
            let stream_result = tokio::select! {
                _ = wake.closed() => return,
                res = client.stream_ops(&folder_id) => res,
            };
            match stream_result {
                Ok(mut stream) => {
                    backoff = Duration::from_secs(1);
                    if degraded {
                        tracing::info!(folder = %folder_id, "op stream reconnected");
                        degraded = false;
                    }
                    // Ends on a closed stream, a read error, or when the engine
                    // drops its receiver.
                    loop {
                        let next = tokio::select! {
                            _ = wake.closed() => return,
                            res = stream.next_head() => res,
                        };
                        match next {
                            Ok(Some(_)) => {
                                if matches!(
                                    wake.try_send(()),
                                    Err(mpsc::error::TrySendError::Closed(_))
                                ) {
                                    return;
                                }
                            }
                            _ => break,
                        }
                    }
                }
                Err(error) => {
                    if !degraded {
                        tracing::warn!(
                            folder = %folder_id,
                            %error,
                            "op stream unavailable; relying on the fallback poll"
                        );
                        degraded = true;
                    }
                }
            }
            tokio::select! {
                _ = wake.closed() => return,
                _ = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    });
    rx
}

/// The pull watermark to store after a push.
///
/// `applied` are the op ids the server assigned to this device's batch, in
/// order. Op ids are one global sequence, so a *foreign* op can be interleaved
/// into our batch — we take 101, another device takes 102, we take 103. The
/// watermark may only move across ops this device has actually applied, so it
/// advances along the longest contiguous run starting at `previous` and stops
/// at the first gap. The next pull then re-fetches from the gap and delivers
/// the foreign op; our own ops after it are applied twice, which is idempotent.
fn advance_watermark(previous: i64, applied: &[i64]) -> i64 {
    let mut watermark = previous;
    for seq in applied {
        if *seq == watermark + 1 {
            watermark = *seq;
        } else {
            break;
        }
    }
    watermark
}

#[cfg(test)]
mod tests {
    use super::{
        advance_watermark, baseline_excluding, changed_since_snapshot, local_changes,
        pull_conflicts,
    };
    use omni_core::{Change, ChunkRef, FileMeta, Ignore, Manifest, changes};

    fn meta(label: &str) -> FileMeta {
        FileMeta {
            size: 3,
            mtime_ns: 1,
            mode: 0,
            chunks: vec![ChunkRef::new(label, 3)],
        }
    }

    fn test_engine(root: &std::path::Path) -> super::Engine {
        super::Engine::new(
            // apply_entries never contacts this address for the test cases:
            // conflicts are preserved, and empty files need no chunks.
            omni_transfer::Client::new("http://127.0.0.1:1").unwrap(),
            omni_store::Store::open_agent_in_memory().unwrap(),
            super::EngineConfig::new("folder", root),
        )
    }

    fn incoming(
        seq: i64,
        bytes: &[u8],
        kind: omni_netproto::OpKindDto,
    ) -> omni_netproto::LogEntryDto {
        omni_netproto::LogEntryDto {
            seq,
            version_id: seq,
            kind,
            path: omni_netproto::encode_path(b"note"),
            device_id: "peer".into(),
            size: bytes.len() as u64,
            mtime_ns: 0,
            mode: 0,
            chunks: if bytes.is_empty() {
                vec![]
            } else {
                vec![omni_netproto::ChunkRefDto {
                    hash: omni_chunk::hash_hex(bytes),
                    len: bytes.len() as u64,
                }]
            },
        }
    }

    #[tokio::test]
    async fn first_sync_conflict_survives_all_historical_operations() {
        use omni_netproto::OpKindDto::{Delete, Put};
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("note"), b"unique local content").unwrap();
        let engine = test_engine(root.path());
        engine
            .apply_entries(
                vec![
                    incoming(1, b"older remote", Put),
                    incoming(2, b"unique local content", Put),
                    incoming(3, b"", Delete),
                ],
                false,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(root.path().join("note")).unwrap(),
            b"unique local content"
        );
        assert_eq!(
            engine
                .store
                .with(|c| omni_store::agent::applied_seq(c, "folder"))
                .unwrap(),
            3
        );
        assert_eq!(
            engine
                .store
                .with(|c| omni_store::agent::versions(c, "folder"))
                .unwrap()
                .get(b"note".as_slice()),
            Some(&0)
        );
    }

    #[tokio::test]
    async fn historical_delete_preserves_untracked_local_content() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("note"), b"local").unwrap();
        let engine = test_engine(root.path());
        engine
            .apply_entries(
                vec![incoming(1, b"", omni_netproto::OpKindDto::Delete)],
                false,
                None,
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read(root.path().join("note")).unwrap(), b"local");
    }

    #[tokio::test]
    async fn clean_file_accepts_remote_replacement_but_local_deletion_is_preserved() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("note"), b"old clean content").unwrap();
        let engine = test_engine(root.path());
        let baseline = engine.local_snapshot(b"note").unwrap().unwrap();
        engine
            .store
            .with(|c| omni_store::agent::set_file(c, "folder", b"note", &baseline, 1))
            .unwrap();
        engine
            .apply_entries(
                vec![incoming(2, b"", omni_netproto::OpKindDto::Put)],
                false,
                None,
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read(root.path().join("note")).unwrap(), b"");
        std::fs::remove_file(root.path().join("note")).unwrap();
        engine
            .apply_entries(
                vec![incoming(
                    3,
                    b"remote replacement",
                    omni_netproto::OpKindDto::Put,
                )],
                false,
                None,
            )
            .await
            .unwrap();
        assert!(!root.path().join("note").exists());
    }

    #[test]
    fn synthetic_conflicts_are_pushed_even_when_content_becomes_equivalent() {
        let mut last = Manifest::new();
        last.insert(b"note".to_vec(), meta("a"));
        let versions = [(b"note".to_vec(), 0)].into_iter().collect();
        assert!(matches!(
            local_changes(&last, &last, &versions).as_slice(),
            [Change::Put { .. }]
        ));
        assert!(matches!(
            local_changes(&last, &Manifest::new(), &versions).as_slice(),
            [Change::Delete { .. }]
        ));
        // An excluded baseline is invisible even when its old version is zero.
        assert!(local_changes(&Manifest::new(), &Manifest::new(), &versions).is_empty());
    }

    #[tokio::test]
    async fn matching_the_final_remote_head_is_adopted_without_an_overwrite() {
        use omni_netproto::OpKindDto::Put;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("note"), b"current head").unwrap();
        let engine = test_engine(root.path());
        engine
            .apply_entries(
                vec![
                    incoming(1, b"older version", Put),
                    incoming(2, b"current head", Put),
                ],
                false,
                None,
            )
            .await
            .unwrap();
        let latest = [(b"note".to_vec(), (2, Put))].into_iter().collect();
        engine.adopt_equivalent_conflicts(&latest).await.unwrap();
        assert_eq!(
            engine
                .store
                .with(|c| omni_store::agent::versions(c, "folder"))
                .unwrap()
                .get(b"note".as_slice()),
            Some(&2)
        );
    }

    #[test]
    fn touching_a_file_during_fetch_is_not_a_content_conflict() {
        let before = meta("same");
        let mut after = before.clone();
        after.mtime_ns += 100;
        assert!(!changed_since_snapshot(Some(&before), Some(&after)));
    }

    #[test]
    fn excluding_a_previously_synced_path_is_not_a_delete() {
        let ignore = Ignore::vault_defaults();
        let mut last = Manifest::new();
        last.insert(b"notes/todo.md".to_vec(), meta("a"));
        last.insert(b".obsidian/workspace.json".to_vec(), meta("b"));

        // The scan skips the newly excluded file, but still sees the note.
        let mut current = Manifest::new();
        current.insert(b"notes/todo.md".to_vec(), meta("a"));

        let diff = changes(&baseline_excluding(last, &ignore), &current);
        assert!(
            diff.is_empty(),
            "an excluded path must not become a Delete, got {diff:?}"
        );
    }

    #[test]
    fn a_path_removed_from_disk_is_still_a_delete() {
        let ignore = Ignore::default();
        let mut last = Manifest::new();
        last.insert(b"notes/todo.md".to_vec(), meta("a"));

        let diff = changes(&baseline_excluding(last, &ignore), &Manifest::new());
        assert!(matches!(
            diff.as_slice(),
            [Change::Delete { path }] if path == b"notes/todo.md"
        ));
    }

    #[test]
    fn a_contiguous_batch_moves_the_watermark_to_its_end() {
        assert_eq!(advance_watermark(0, &[1, 2, 3]), 3);
        assert_eq!(advance_watermark(7, &[8, 9]), 9);
    }

    #[test]
    fn a_foreign_op_inside_the_batch_stops_the_watermark_at_the_gap() {
        // We pushed 101 and 103; another device took 102 in between.
        assert_eq!(advance_watermark(100, &[101, 103]), 101);
        // Our first op is already foreign-contaminated: nothing advances.
        assert_eq!(advance_watermark(100, &[102, 103]), 100);
    }

    #[test]
    fn a_first_push_only_advances_when_the_folder_was_empty() {
        assert_eq!(advance_watermark(0, &[1, 2]), 2);
        // The folder already held ops from another device: ours start past them.
        assert_eq!(advance_watermark(0, &[51, 52]), 0);
    }

    #[test]
    fn an_empty_batch_leaves_the_watermark_alone() {
        assert_eq!(advance_watermark(42, &[]), 42);
    }

    #[test]
    fn pull_guard_compares_with_local_baseline_not_incoming_remote() {
        let clean = meta("base");
        let remote_update = meta("remote");
        assert!(!pull_conflicts(Some(&clean), Some(&clean)));
        assert!(pull_conflicts(Some(&clean), Some(&remote_update)));
        assert!(
            pull_conflicts(Some(&clean), None),
            "local deletion is dirty"
        );
        assert!(
            pull_conflicts(None, Some(&remote_update)),
            "untracked local put is dirty"
        );
        assert!(
            !pull_conflicts(None, None),
            "absent path can accept a remote put/delete"
        );
    }

    #[test]
    fn local_snapshot_detects_edit_and_deletion_during_fetch() {
        let before = Some(meta("base"));
        assert!(changed_since_snapshot(
            before.as_ref(),
            Some(&meta("edited"))
        ));
        assert!(changed_since_snapshot(before.as_ref(), None));
        assert!(!changed_since_snapshot(
            before.as_ref(),
            Some(&meta("base"))
        ));
        assert!(changed_since_snapshot(None, Some(&meta("new local put"))));
        assert!(!changed_since_snapshot(None, None));
    }
}
