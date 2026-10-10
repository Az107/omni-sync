//! The sync engine: push local changes, then pull remote ones.
//!
//! One cycle is deliberately simple:
//! 1. scan the root into a manifest;
//! 2. diff it against the last-synced state → ops to **push** (upload chunks,
//!    submit ops, record the new version ids);
//! 3. pull the op log since our watermark and **apply** each entry.
//!
//! Running it on a short interval *is* the watcher for now; the `notify` handle
//! in [`crate::watcher`] can shorten the interval later without touching this.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use omni_chunk::Chunker;
use omni_core::{Change, FileMeta, Op, OpKind, PathBytes, changes};
use omni_netproto::{OpDto, OpKindDto, decode_chunks, decode_path};
use omni_store::{Store, agent};
use omni_transfer::Client;

use crate::{apply, scan};

/// One folder to keep in sync.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub folder_id: String,
    pub root: PathBuf,
    pub interval: Duration,
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

    /// Run forever, logging and surviving cycle failures.
    pub async fn run(&self) -> Result<()> {
        loop {
            if let Err(error) = self.sync_once().await {
                tracing::warn!(folder = %self.config.folder_id, %error, "sync cycle failed");
            }
            tokio::time::sleep(self.config.interval).await;
        }
    }

    /// One push+pull cycle.
    pub async fn sync_once(&self) -> Result<()> {
        self.push().await?;
        self.pull().await?;
        Ok(())
    }

    async fn push(&self) -> Result<()> {
        // 1. Scan the root.
        let (root, chunker) = (self.config.root.clone(), self.chunker);
        let current = tokio::task::spawn_blocking(move || scan::scan(&root, &chunker))
            .await
            .context("scan task panicked")??;

        // 2. What we last synced, and each path's version.
        let folder = self.config.folder_id.clone();
        let store = self.store.clone();
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

        let local = changes(&last, &current);
        if local.is_empty() {
            return Ok(());
        }

        // 3. Turn each change into an op, uploading chunks for puts.
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

        let dtos = pending.iter().map(|p| p.dto.clone()).collect();
        let response = self
            .client
            .submit_ops(&self.config.folder_id, dtos)
            .await
            .context("submitting ops")?;

        // 4. Record success: the new version ids and the advanced watermark.
        let pushed = pending.len();
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

        tracing::info!(
            folder = %self.config.folder_id,
            pushed,
            "pushed changes"
        );
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
                    let meta = FileMeta {
                        size: entry.size,
                        mtime_ns: entry.mtime_ns,
                        mode: entry.mode,
                        chunks: decode_chunks(&entry.chunks),
                    };
                    apply::write_file(&self.config.root, &path, &bytes, &meta)
                        .with_context(|| format!("writing {}", String::from_utf8_lossy(&path)))?;
                    let folder = folder.clone();
                    let stored = meta.clone();
                    self.store
                        .blocking(move |conn| {
                            agent::set_file(conn, &folder, &path, &stored, entry.version_id)
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
