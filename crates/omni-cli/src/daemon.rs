//! Daemon lifecycle: live folder-set reload and the local control channel.
//!
//! `omni-sync daemon` owns one [`Engine`] task per configured folder. It
//! watches its own config file (plus a 1 s fallback check) so `omni-sync add`
//! or a manual edit takes effect without restarting the daemon, and it listens
//! on an authenticated loopback socket (`<stem>.daemon.ctl`, mode `0600`) so
//! `omni-sync add` and `omni-sync flush` can ask the running daemon to sync
//! immediately without opening a competing `Engine` on the same database.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use omni_agent::{Engine, FlushRequest, path_bytes};
use omni_store::Store;
use rand::Rng as _;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::{ClientConfig, WatchEntry, agent_db_path, engine_config, load_config, write_private};

const CONFIG_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// What changed between the currently running folder set and a newly loaded
/// config. Pure, so it is unit-tested in isolation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FolderDiff {
    pub to_stop: Vec<String>,
    pub to_start: Vec<WatchEntry>,
}

/// Deduplicate `desired` by `folder_id` (keeping the last entry) and compute
/// which running engines must stop and which must start.
pub fn diff_folders(running: &BTreeMap<String, WatchEntry>, desired: &[WatchEntry]) -> FolderDiff {
    let mut deduped = BTreeMap::new();
    for entry in desired {
        deduped.insert(entry.folder_id.clone(), entry.clone());
    }

    let mut to_stop = Vec::new();
    for (folder_id, active) in running {
        match deduped.get(folder_id) {
            Some(next) if next == active => {}
            _ => to_stop.push(folder_id.clone()),
        }
    }

    let mut to_start = Vec::new();
    for (folder_id, next) in deduped {
        match running.get(&folder_id) {
            Some(active) if active == &next => {}
            _ => to_start.push(next),
        }
    }

    FolderDiff { to_stop, to_start }
}

/// Connection details written to `<stem>.daemon.ctl` (mode `0600`) while a
/// daemon is running.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ControlInfo {
    addr: String,
    token: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ControlCommand {
    /// Reload the config and run an immediate sync on `folder_id` (or all
    /// folders when `None`).
    ReloadAndSync { folder_id: Option<String> },
    /// Override holds and run an immediate sync on `folder` (or all folders)
    /// and optional relative `path`.
    Flush {
        folder: Option<String>,
        path: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ControlWireRequest {
    token: String,
    command: ControlCommand,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ControlWireResponse {
    ok: bool,
    synced: usize,
    #[serde(default)]
    error: Option<String>,
}

struct RunningEngine {
    entry: WatchEntry,
    control: mpsc::Sender<FlushRequest>,
    handle: JoinHandle<()>,
}

struct FolderManager {
    store: Store,
    running: BTreeMap<String, RunningEngine>,
}

impl FolderManager {
    fn new(store: Store) -> Self {
        Self {
            store,
            running: BTreeMap::new(),
        }
    }

    fn running_entries(&self) -> BTreeMap<String, WatchEntry> {
        self.running
            .iter()
            .map(|(id, item)| (id.clone(), item.entry.clone()))
            .collect()
    }

    /// Reconcile running engine tasks against `config.folders`.
    async fn reconcile(&mut self, config: &ClientConfig) -> Result<()> {
        let client = omni_transfer::Client::new(&config.server)?.with_token(config.token.clone());
        // Best-effort check against the server's folder list: if the server is
        // reachable, skip stale folder ids that do not exist on the server. If
        // the server is unreachable (offline), keep all entries so they sync
        // once connectivity returns.
        let server_folders: Option<BTreeSet<String>> = client
            .list_folders()
            .await
            .ok()
            .map(|list| list.into_iter().map(|f| f.id).collect());

        let mut valid = Vec::new();
        for entry in &config.folders {
            if let Err(error) = engine_config(entry) {
                tracing::warn!(
                    folder = %entry.folder_id,
                    %error,
                    "skipping folder with invalid config"
                );
                continue;
            }
            if let Some(known) = &server_folders
                && !known.contains(&entry.folder_id)
            {
                tracing::warn!(
                    folder = %entry.folder_id,
                    "folder does not exist on the server; skipping"
                );
                continue;
            }
            valid.push(entry.clone());
        }

        let diff = diff_folders(&self.running_entries(), &valid);
        for folder_id in diff.to_stop {
            if let Some(stopped) = self.running.remove(&folder_id) {
                tracing::info!(folder = %folder_id, "stopping folder engine");
                stopped.handle.abort();
            }
        }

        for entry in diff.to_start {
            let Ok(cfg) = engine_config(&entry) else {
                continue;
            };
            let client =
                omni_transfer::Client::new(&config.server)?.with_token(config.token.clone());
            let engine = Engine::new(client, self.store.clone(), cfg);
            let (tx, rx) = mpsc::channel(8);
            let folder_id = entry.folder_id.clone();
            let display_path = entry.path.display().to_string();
            let mode = entry.mode.clone();
            tracing::info!(folder = %folder_id, path = %display_path, mode = %mode, "syncing");
            let handle = tokio::spawn(async move {
                if let Err(error) = engine.run_with_control(Some(rx)).await {
                    tracing::error!(folder = %folder_id, %error, "sync engine stopped");
                }
            });
            self.running.insert(
                entry.folder_id.clone(),
                RunningEngine {
                    entry,
                    control: tx,
                    handle,
                },
            );
        }
        Ok(())
    }

    /// Ask matching running engines to run a sync cycle and wait for completion.
    async fn trigger(
        &self,
        folder: Option<&str>,
        path: Option<&str>,
        override_holds: bool,
    ) -> Result<usize> {
        let mut targets = Vec::new();
        for (id, running) in &self.running {
            let matched = match folder {
                None => true,
                Some(filter) => id == filter || running.entry.path.as_path() == Path::new(filter),
            };
            if matched {
                targets.push((id.clone(), running.control.clone()));
            }
        }

        if folder.is_some() && targets.is_empty() {
            bail!(
                "folder '{}' is not being watched by this daemon",
                folder.unwrap_or_default()
            );
        }

        let path_bytes = path.map(|p| path_bytes(Path::new(p)));
        let mut count = 0;
        for (folder_id, tx) in targets {
            let (reply_tx, reply_rx) = oneshot::channel();
            tx.send(FlushRequest {
                path: path_bytes.clone(),
                override_holds,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow!("engine for folder {folder_id} is not running"))?;
            reply_rx
                .await
                .map_err(|_| anyhow!("engine for folder {folder_id} dropped its reply"))??;
            count += 1;
        }
        Ok(count)
    }

    fn stop_all(&mut self) {
        for (_, running) in std::mem::take(&mut self.running) {
            running.handle.abort();
        }
    }
}

pub fn control_file_path(config_path: &Path) -> PathBuf {
    let stem = config_path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("client");
    let dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!("{stem}.daemon.ctl"))
}

/// Send a command to the running daemon for `config_path`, if one is alive.
/// Returns `Ok(None)` when no daemon is running so the caller can fall back to
/// an in-process one-shot run.
pub async fn try_send_command(
    config_path: &Path,
    command: ControlCommand,
) -> Result<Option<usize>> {
    let ctl_path = control_file_path(config_path);
    let Ok(raw) = std::fs::read_to_string(&ctl_path) else {
        return Ok(None);
    };
    let Ok(info) = serde_json::from_str::<ControlInfo>(&raw) else {
        return Ok(None);
    };
    let Ok(mut stream) = TcpStream::connect(&info.addr).await else {
        // Stale control file from a killed daemon.
        let _ = std::fs::remove_file(&ctl_path);
        return Ok(None);
    };

    let wire = ControlWireRequest {
        token: info.token,
        command,
    };
    let mut payload = serde_json::to_vec(&wire)?;
    payload.push(b'\n');
    stream.write_all(&payload).await?;
    stream.shutdown().await?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    if line.trim().is_empty() {
        return Err(anyhow!(
            "daemon closed the control connection without replying"
        ));
    }
    let response: ControlWireResponse = serde_json::from_str(&line)?;
    if !response.ok {
        bail!(
            "{}",
            response
                .error
                .unwrap_or_else(|| "daemon refused the command".to_string())
        );
    }
    Ok(Some(response.synced))
}

/// Run the client daemon until Ctrl-C, watching `config_path` for changes and
/// serving `add` / `flush` requests on the local control socket.
pub async fn run(config_path: &Path) -> Result<()> {
    let initial = load_config(config_path)?;
    let store = Store::open_agent(&agent_db_path(config_path))?;
    let mut manager = FolderManager::new(store);
    manager.reconcile(&initial).await?;

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("binding daemon control listener")?;
    let addr = listener.local_addr()?.to_string();
    let mut token_bytes = [0u8; 32];
    rand::rng().fill(&mut token_bytes);
    let token = hex::encode(token_bytes);

    let ctl_path = control_file_path(config_path);
    let ctl_json = serde_json::to_string(&ControlInfo {
        addr,
        token: token.clone(),
    })?;
    write_private(&ctl_path, &ctl_json)?;

    let (_watcher, mut config_events) = watch_config_dir(config_path);
    let mut poll = tokio::time::interval(CONFIG_POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    poll.tick().await;

    let mut last_raw = std::fs::read_to_string(config_path).unwrap_or_default();

    println!(
        "omni-sync daemon: {} folder(s); watching for changes, streaming the op log",
        manager.running.len()
    );

    loop {
        tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => {
                break;
            }
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    handle_control_client(
                        stream,
                        &token,
                        config_path,
                        &mut last_raw,
                        &mut manager,
                    )
                    .await;
                }
            }
            Some(()) = config_events.recv() => {
                reload_if_changed(config_path, &mut last_raw, &mut manager).await;
            }
            _ = poll.tick() => {
                reload_if_changed(config_path, &mut last_raw, &mut manager).await;
            }
        }
    }

    manager.stop_all();
    let _ = std::fs::remove_file(&ctl_path);
    Ok(())
}

async fn reload_if_changed(config_path: &Path, last_raw: &mut String, manager: &mut FolderManager) {
    let Ok(raw) = std::fs::read_to_string(config_path) else {
        return;
    };
    if raw == *last_raw {
        return;
    }
    match toml::from_str::<ClientConfig>(&raw) {
        Ok(config) => {
            *last_raw = raw;
            if let Err(error) = manager.reconcile(&config).await {
                tracing::warn!(%error, "failed to reconcile reloaded config");
            }
        }
        Err(error) => {
            tracing::warn!(%error, "ignoring invalid config reload");
        }
    }
}

async fn handle_control_client(
    stream: TcpStream,
    expected_token: &str,
    config_path: &Path,
    last_raw: &mut String,
    manager: &mut FolderManager,
) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).await.is_err() {
        return;
    }
    let mut stream = reader.into_inner();

    let reply = match serde_json::from_str::<ControlWireRequest>(&line) {
        Ok(req) if req.token == expected_token => {
            // Always pick up any latest config edits first.
            if let Ok(raw) = std::fs::read_to_string(config_path)
                && let Ok(config) = toml::from_str::<ClientConfig>(&raw)
            {
                *last_raw = raw;
                let _ = manager.reconcile(&config).await;
            }

            let outcome = match req.command {
                ControlCommand::ReloadAndSync { folder_id } => {
                    manager.trigger(folder_id.as_deref(), None, false).await
                }
                ControlCommand::Flush { folder, path } => {
                    manager
                        .trigger(folder.as_deref(), path.as_deref(), true)
                        .await
                }
            };
            match outcome {
                Ok(synced) => ControlWireResponse {
                    ok: true,
                    synced,
                    error: None,
                },
                Err(error) => ControlWireResponse {
                    ok: false,
                    synced: 0,
                    error: Some(error.to_string()),
                },
            }
        }
        Ok(_) => ControlWireResponse {
            ok: false,
            synced: 0,
            error: Some("unauthorized control request".to_string()),
        },
        Err(error) => ControlWireResponse {
            ok: false,
            synced: 0,
            error: Some(format!("bad control request: {error}")),
        },
    };

    if let Ok(mut bytes) = serde_json::to_vec(&reply) {
        bytes.push(b'\n');
        let _ = stream.write_all(&bytes).await;
        let _ = stream.shutdown().await;
    }
}

/// Watch the directory containing `config_path` so write-then-rename updates
/// wake the daemon immediately.
fn watch_config_dir(config_path: &Path) -> (Option<RecommendedWatcher>, mpsc::Receiver<()>) {
    let (tx, rx) = mpsc::channel(1);
    let dir = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let config_name = config_path.file_name().map(|n| n.to_os_string());
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            let is_mutation = matches!(
                event.kind,
                notify::EventKind::Create(_)
                    | notify::EventKind::Modify(_)
                    | notify::EventKind::Remove(_)
            );
            let matches_config = match &config_name {
                Some(name) => event
                    .paths
                    .iter()
                    .any(|p| p.file_name() == Some(name.as_os_str())),
                None => true,
            };
            if is_mutation && matches_config {
                let _ = tx.try_send(());
            }
        }
    })
    .and_then(|mut w| {
        w.watch(dir, RecursiveMode::NonRecursive)?;
        Ok(w)
    })
    .ok();
    (watcher, rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, path: &str, mode: &str) -> WatchEntry {
        WatchEntry {
            folder_id: id.into(),
            path: PathBuf::from(path),
            mode: mode.into(),
            hold_while_process: Vec::new(),
            quiet_timeout_secs: None,
        }
    }

    #[test]
    fn diff_identifies_added_removed_and_modified_folders() {
        let mut running = BTreeMap::new();
        running.insert("f1".into(), entry("f1", "/a", "live"));
        running.insert("f2".into(), entry("f2", "/b", "live"));
        running.insert("f3".into(), entry("f3", "/c", "live"));

        let desired = vec![
            // f1 unchanged
            entry("f1", "/a", "live"),
            // f2 changed mode -> stop and restart
            entry("f2", "/b", "on_close"),
            // f3 removed, f4 added
            entry("f4", "/d", "live"),
        ];

        let diff = diff_folders(&running, &desired);
        assert_eq!(diff.to_stop, vec!["f2".to_string(), "f3".to_string()]);
        assert_eq!(
            diff.to_start,
            vec![entry("f2", "/b", "on_close"), entry("f4", "/d", "live")]
        );
    }

    #[test]
    fn duplicate_entries_in_config_collapse_to_the_last_one() {
        let running = BTreeMap::new();
        let desired = vec![entry("f1", "/old", "live"), entry("f1", "/new", "on_close")];
        let diff = diff_folders(&running, &desired);
        assert!(diff.to_stop.is_empty());
        assert_eq!(diff.to_start, vec![entry("f1", "/new", "on_close")]);
    }
}
