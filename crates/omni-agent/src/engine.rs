//! The sync engine.
//!
//! Push and pull run on **separate cadences**, because they cost wildly
//! different amounts:
//!
//! - **push** needs to walk and hash the tree, so it is *event-driven* — the
//!   `notify` watcher fires it on a change, with a slow safety scan (30 s) to
//!   catch anything the watcher missed (edits while the daemon was down).
//! - **pull** is a single HTTP GET of the op log, so it runs on a short timer
//!   (2 s) to keep remote-change latency low.
//!
//! The scan itself only reads files whose size/mtime/mode changed since the
//! last sync; unchanged files reuse their known chunk list without a read.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use omni_chunk::Chunker;
use omni_core::{Change, FileMeta, Op, OpKind, PathBytes, changes};
use omni_netproto::{OpDto, OpKindDto, decode_chunks, decode_path};
use omni_store::{Store, agent};
use omni_transfer::Client;

use crate::{apply, scan};

/// How often the cheap op-log pull runs.
const PULL_INTERVAL: Duration = Duration::from_secs(2);
/// The slow full-tree safety scan, for changes the watcher did not see.
const SAFETY_SCAN_INTERVAL: Duration = Duration::from_secs(30);
/// Quiet period after a filesystem event before scanning (coalesces save bursts).
const DEBOUNCE: Duration = Duration::from_millis(250);

/// One folder to keep in sync.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub folder_id: String,
    pub root: PathBuf,
}

/// Drives one folder against one server.
pub struct Engine {
    client: Client,
    store: Store,
    chunker: Chunker,
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
        Self {
            client,
            store,
            chunker: Chunker::new(omni_chunk::Config::default()),
            config,
        }
    }

    /// Run until the watcher's channel closes, logging and surviving cycle
    /// failures. Push is watcher-driven; pull is on a short timer.
    pub async fn run(&self) -> Result<()> {
        let (_handle, mut events) = crate::watcher::watch(&self.config.root)
            .with_context(|| format!("watching {}", self.config.root.display()))?;

        let mut pull = tokio::time::interval(PULL_INTERVAL);
        let mut safety = tokio::time::interval(SAFETY_SCAN_INTERVAL);
        pull.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        safety.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                biased;
                event = events.recv() => {
                    if event.is_none() {
                        return Ok(());
                    }
                    // Let a burst of writes settle, then drain the rest.
                    tokio::time::sleep(DEBOUNCE).await;
                    while events.try_recv().is_ok() {}
                    self.logged("push", self.push()).await;
                }
                _ = safety.tick() => {
                    self.logged("safety scan", self.push()).await;
                }
                _ = pull.tick() => {
                    self.logged("pull", self.pull()).await;
                }
            }
        }
    }

    async fn logged(&self, what: &str, work: impl std::future::Future<Output = Result<()>>) {
        if let Err(error) = work.await {
            tracing::warn!(folder = %self.config.folder_id, %error, "{} failed", what);
        }
    }

    /// One push+pull cycle, for tests and manual runs.
    pub async fn sync_once(&self) -> Result<()> {
        self.push().await?;
        self.pull().await?;
        Ok(())
    }

    async fn push(&self) -> Result<()> {
        let folder = self.config.folder_id.clone();
        let store = self.store.clone();

        // What we last synced, each path's version, and our watermark.
        let (last, versions, applied_seq) = {
            let (f1, f2, f3) = (folder.clone(), folder.clone(), folder.clone());
            let last = store
                .blocking(move |conn| agent::manifest(conn, &f1))
                .await?;
            let versions = store
                .blocking(move |conn| agent::versions(conn, &f2))
                .await?;
            let seq = store
                .blocking(move |conn| agent::applied_seq(conn, &f3))
                .await?;
            (last, versions, seq)
        };

        // Scan, reusing the last-synced chunks for files whose metadata is
        // unchanged (so an idle folder does no disk reads at all).
        let (root, chunker, previous) = (self.config.root.clone(), self.chunker, last.clone());
        let current = tokio::task::spawn_blocking(move || scan::scan(&root, &chunker, &previous))
            .await
            .context("scan task panicked")??;

        let local = changes(&last, &current);
        if local.is_empty() {
            return Ok(());
        }

        let mut pending = Vec::new();
        for change in &local {
            match change {
                Change::Put { path, .. } => {
                    let (root, rel) = (self.config.root.clone(), path.clone());
                    let chunker = self.chunker;
                    let (meta, hashed) = tokio::task::spawn_blocking(move || {
                        scan::chunk_file(&root, &rel, &chunker)
                    })
                    .await
                    .context("chunk task panicked")??;
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

        let pushed = pending.len();
        let dtos = pending.iter().map(|item| item.dto.clone()).collect();
        let response = self
            .client
            .submit_ops(&self.config.folder_id, dtos)
            .await
            .context("submitting ops")?;

        let store = self.store.clone();
        let folder = self.config.folder_id.clone();
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
                if let Some(last) = response.applied.last() {
                    agent::set_applied_seq(conn, &folder, last.seq)?;
                }
                Ok(())
            })
            .await?;

        tracing::info!(folder = %self.config.folder_id, pushed, "pushed changes");
        Ok(())
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

    async fn pull(&self) -> Result<()> {
        let folder = self.config.folder_id.clone();
        let since = {
            let f = folder.clone();
            self.store
                .blocking(move |conn| agent::applied_seq(conn, &f))
                .await?
        };

        let response = self.client.ops_since(&self.config.folder_id, since).await?;
        for entry in response.entries {
            let path = decode_path(&entry.path).map_err(|_| anyhow!("server sent a bad path"))?;

            match entry.kind {
                OpKindDto::Delete => {
                    apply::remove_file(&self.config.root, &path)
                        .with_context(|| format!("deleting {}", String::from_utf8_lossy(&path)))?;
                    let folder = folder.clone();
                    self.store
                        .blocking(move |conn| agent::remove_file(conn, &folder, &path))
                        .await?;
                }
                OpKindDto::Put | OpKindDto::Resolve | OpKindDto::Move => {
                    let bytes = self.fetch_chunks(&entry.chunks).await?;
                    apply::write_file(
                        &self.config.root,
                        &path,
                        &bytes,
                        &FileMeta {
                            size: entry.size,
                            mtime_ns: entry.mtime_ns,
                            mode: entry.mode,
                            chunks: decode_chunks(&entry.chunks),
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
                        mode: entry.mode,
                        chunks: decode_chunks(&entry.chunks),
                    };
                    let folder = folder.clone();
                    let version_id = entry.version_id;
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
        Ok(())
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
            bytes.extend_from_slice(&data);
        }
        Ok(bytes)
    }
}
