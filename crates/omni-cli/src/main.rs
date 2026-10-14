//! The `omni-sync` client binary.
//!
//! `enroll` registers the device (by pairing code or magic link); `add` records
//! a folder to watch and runs its initial sync; `daemon` runs the watcher/sync
//! loop for every recorded folder and reloads when the config changes; `flush`
//! forces an immediate sync cycle; `update` installs a newer release from
//! GitHub.

mod daemon;
mod link;
mod wizard;

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use clap::{Parser, Subcommand};
use directories::ProjectDirs;
use omni_agent::{Engine, EngineConfig, HoldRule, SyncMode, path_bytes};
use omni_netproto::FolderDto;
use omni_store::Store;

#[derive(Parser)]
#[command(
    name = "omni-sync",
    version,
    about = "omni-sync client: watch and sync folders"
)]
struct Cli {
    /// Path to the client config. Defaults to the per-user config directory.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Register this device. Prompt-free with --server/--code.
    Enroll {
        /// Server base URL, e.g. https://sync.example.com (or .../app/omni-sync).
        #[arg(long)]
        server: Option<String>,
        /// One-time pairing code from the dashboard.
        #[arg(long)]
        code: Option<String>,
        /// Magic link from the dashboard; opens a browser and takes the code.
        #[arg(long)]
        link: Option<String>,
        /// A human label for this device. Defaults to the hostname.
        #[arg(long)]
        name: Option<String>,
        /// With --link, enroll directly instead of opening a browser.
        #[arg(long)]
        no_browser: bool,
        /// Seconds to wait for the browser to finish enrolling.
        #[arg(long, default_value_t = 180)]
        timeout: u64,
    },
    /// Watch a shared folder (by id or name) at a local path. Prompts if args are omitted.
    Add {
        /// Folder id or name from `omni-sync folders`; prompted for if omitted.
        folder: Option<String>,
        /// Local directory to watch; prompted for if omitted.
        path: Option<PathBuf>,
        /// Sync mode: `live` or `on_close`.
        #[arg(long)]
        mode: Option<String>,
        /// Create the folder on the server if it does not exist yet.
        #[arg(long, visible_alias = "new")]
        create: bool,
        /// Process name globs that hold this folder while running (e.g. `steam*`).
        #[arg(long = "hold-while-process", value_name = "GLOB")]
        hold_while_process: Vec<String>,
        /// Quiet timeout in seconds for `on_close` holds.
        #[arg(long = "quiet-timeout", value_name = "SECS")]
        quiet_timeout_secs: Option<u64>,
    },
    /// Manage shared folders on the server (`create`, `list`).
    Folder {
        #[command(subcommand)]
        action: FolderAction,
    },
    /// Run the watcher/sync daemon.
    Daemon,
    /// List folders this server knows about.
    Folders,
    /// Show per-folder sync state and any held files.
    Status {
        /// Filter to a single folder (by id or local path).
        #[arg(long)]
        folder: Option<String>,
    },
    /// Print the resolved config and store status.
    Doctor,
    /// Install a newer release from GitHub.
    Update {
        /// Only report whether a newer release exists.
        #[arg(long)]
        check: bool,
        /// Reinstall the release even if this binary is already current.
        #[arg(long)]
        force: bool,
        /// A specific release tag (e.g. v0.2.0) instead of the latest.
        #[arg(long)]
        to: Option<String>,
    },
    /// Print the version chain for a file.
    History { folder: String, path: String },
    /// Roll a path back to a version.
    Resolve {
        folder: String,
        path: String,
        #[arg(long = "to")]
        version: i64,
    },
    /// Override a hold and sync now.
    #[command(visible_alias = "sync")]
    Flush {
        folder: Option<String>,
        path: Option<String>,
    },
}

#[derive(Subcommand)]
enum FolderAction {
    /// Create a shared folder on the server and print its id.
    Create { name: String },
    /// List folders on the server.
    List,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ClientConfig {
    pub(crate) server: String,
    pub(crate) device_id: String,
    pub(crate) token: String,
    #[serde(default)]
    pub(crate) folders: Vec<WatchEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct WatchEntry {
    pub(crate) folder_id: String,
    pub(crate) path: PathBuf,
    #[serde(default = "default_mode")]
    pub(crate) mode: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) hold_while_process: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) quiet_timeout_secs: Option<u64>,
}

fn default_mode() -> String {
    "live".to_string()
}

pub(crate) fn engine_config(entry: &WatchEntry) -> anyhow::Result<EngineConfig> {
    let mode: SyncMode = entry.mode.parse().map_err(|error: String| anyhow!(error))?;
    let hold_rule = HoldRule::new(
        entry.hold_while_process.clone(),
        entry.quiet_timeout_secs.unwrap_or(0),
    );
    Ok(
        EngineConfig::new(entry.folder_id.clone(), entry.path.clone())
            .with_mode(mode)
            .with_hold_rule(hold_rule),
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(|| client_config_path(None));
    match cli.command {
        Command::Enroll {
            server,
            code,
            link,
            name,
            no_browser,
            timeout,
        } => {
            enroll(
                &config_path,
                server,
                code,
                link,
                name,
                no_browser,
                Duration::from_secs(timeout),
            )
            .await
        }
        Command::Add {
            folder,
            path,
            mode,
            create,
            hold_while_process,
            quiet_timeout_secs,
        } => {
            add(
                &config_path,
                folder,
                path,
                mode,
                create,
                hold_while_process,
                quiet_timeout_secs,
            )
            .await
        }
        Command::Folder { action } => folder_cmd(&config_path, action).await,
        Command::Daemon => daemon::run(&config_path).await,
        Command::Folders => folders(&config_path).await,
        Command::Status { folder } => status(&config_path, folder.as_deref()),
        Command::Doctor => doctor(&config_path).await,
        Command::Update { check, force, to } => update(check, force, to).await,
        Command::History { folder, path } => history(&config_path, &folder, &path).await,
        Command::Resolve {
            folder,
            path,
            version,
        } => resolve(&config_path, &folder, &path, version).await,
        Command::Flush { folder, path } => {
            flush(&config_path, folder.as_deref(), path.as_deref()).await
        }
    }
}

/// True when we may prompt: a terminal on stdin, and not asked to be quiet.
fn interactive() -> bool {
    std::io::stdin().is_terminal()
}

/// A sensible default device name.
fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|name| name.trim().to_string())
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "device".to_string())
}

async fn enroll(
    config_path: &Path,
    server: Option<String>,
    code: Option<String>,
    link: Option<String>,
    name: Option<String>,
    no_browser: bool,
    timeout: Duration,
) -> anyhow::Result<()> {
    // A magic link carries both the server and the code; the loopback handoff
    // resolves the code before we enroll.
    let via_link = link.is_some();
    let (server, code) = if let Some(link) = link {
        let resolved = self::link::resolve(&link, no_browser, timeout).await?;
        (resolved.server, resolved.code)
    } else {
        match (server, code) {
            (Some(server), Some(code)) => (server, code),
            (server, code) if interactive() => {
                let server = match server {
                    Some(server) => server,
                    None => wizard::prompt("Server URL", None)?,
                };
                let code = match code {
                    Some(code) => code,
                    None => wizard::prompt("Pairing code", None)?,
                };
                (server, code)
            }
            _ => bail!("--server and --code are required when not on a terminal"),
        }
    };

    let name = match name {
        Some(name) => name,
        // A magic-link enrollment should be hands-off: no prompt after the
        // browser handoff, just the hostname.
        None if interactive() && !via_link => wizard::prompt("Device name", Some(&hostname()))?,
        None => hostname(),
    };

    let client = omni_transfer::Client::new(&server)?;
    let response = client.enroll(&name, &code).await?;

    let config = ClientConfig {
        server,
        device_id: response.device_id.clone(),
        token: response.token,
        folders: Vec::new(),
    };
    save_config(config_path, &config)?;
    println!("enrolled as device {}", config.device_id);
    println!("config written to {}", config_path.display());
    println!("next: `omni-sync add` to pick a folder to watch.");
    Ok(())
}

/// Match a user-supplied folder identifier against the server's folder list by
/// exact id first, then by unambiguous name.
fn match_folder(folders: &[FolderDto], query: &str) -> anyhow::Result<Option<FolderDto>> {
    if let Some(exact) = folders.iter().find(|folder| folder.id == query) {
        return Ok(Some(exact.clone()));
    }
    let by_name: Vec<&FolderDto> = folders
        .iter()
        .filter(|folder| folder.name == query)
        .collect();
    match by_name.as_slice() {
        [] => Ok(None),
        [single] => Ok(Some((*single).clone())),
        _ => {
            bail!("multiple folders on the server are named '{query}'; pass the folder id instead")
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn add(
    config_path: &Path,
    folder: Option<String>,
    path: Option<PathBuf>,
    mode: Option<String>,
    create: bool,
    hold_while_process: Vec<String>,
    quiet_timeout_secs: Option<u64>,
) -> anyhow::Result<()> {
    let mut config = load_config(config_path)?;
    let client = omni_transfer::Client::new(&config.server)?.with_token(config.token.clone());

    let mode = match mode {
        Some(mode) => {
            let parsed: SyncMode = mode.parse().map_err(|error: String| anyhow!(error))?;
            parsed.as_str().to_string()
        }
        None if interactive() => {
            if wizard::confirm("Sync in live mode? (no = on_close)", true)? {
                "live".to_string()
            } else {
                "on_close".to_string()
            }
        }
        None => "live".to_string(),
    };

    // Resolve or create the server folder *before* mutating local state.
    let server_folders = client
        .list_folders()
        .await
        .context("listing folders on the server")?;
    let folder_id = match folder {
        Some(query) => match match_folder(&server_folders, &query)? {
            Some(found) => found.id,
            None if create => {
                let created = client
                    .create_folder(&query)
                    .await
                    .with_context(|| format!("creating folder '{query}'"))?;
                println!("created folder {} ({})", created.name, created.id);
                created.id
            }
            None if interactive() => {
                if wizard::confirm(
                    &format!("Folder '{query}' does not exist on the server; create it?"),
                    true,
                )? {
                    let created = client
                        .create_folder(&query)
                        .await
                        .with_context(|| format!("creating folder '{query}'"))?;
                    println!("created folder {} ({})", created.name, created.id);
                    created.id
                } else {
                    bail!("folder '{query}' does not exist on the server");
                }
            }
            None => bail!(
                "folder '{query}' does not exist on the server (create it with `omni-sync folder create <name>` or pass `--create`)"
            ),
        },
        None if interactive() => match wizard::select_or_create_folder(&server_folders)? {
            wizard::FolderChoice::Existing(id) => id,
            wizard::FolderChoice::CreateNew(name) => {
                let created = client
                    .create_folder(&name)
                    .await
                    .with_context(|| format!("creating folder '{name}'"))?;
                println!("created folder {} ({})", created.name, created.id);
                created.id
            }
        },
        None => bail!(
            "a folder id is required when not on a terminal; run `omni-sync folders` or pass one"
        ),
    };

    // Which local directory?
    let path = match path {
        Some(path) => path,
        None if interactive() => wizard::prompt_path("Local directory to watch")?,
        None => bail!("a local path is required when not on a terminal"),
    };
    let path = prepare_path(&path, interactive())?;

    if interactive()
        && !wizard::confirm(
            &format!(
                "Watch folder {folder_id} at {} ({mode} mode)?",
                path.display()
            ),
            true,
        )?
    {
        println!("cancelled");
        return Ok(());
    }

    // Record membership on the server before writing the local config so a
    // rejected folder never leaves a broken entry in `client.toml`.
    let root = path_bytes(&path);
    client
        .register_member(&folder_id, &root, "both")
        .await
        .with_context(|| format!("registering membership for folder {folder_id}"))?;

    let new_entry = WatchEntry {
        folder_id: folder_id.clone(),
        path: path.clone(),
        mode,
        hold_while_process,
        quiet_timeout_secs,
    };
    let cfg = engine_config(&new_entry)?;

    config.folders.retain(|entry| entry.folder_id != folder_id);
    config.folders.push(new_entry);
    save_config(config_path, &config)?;

    // Start syncing immediately: if a daemon is running, tell it to reload its
    // config and sync the new folder; otherwise run one initial sync cycle
    // in-process so existing files move right away.
    match daemon::try_send_command(
        config_path,
        daemon::ControlCommand::ReloadAndSync {
            folder_id: Some(folder_id.clone()),
        },
    )
    .await
    {
        Ok(Some(_)) => {}
        Ok(None) => {
            let store = Store::open_agent(&agent_db_path(config_path))?;
            let engine = Engine::new(client, store, cfg);
            if let Err(error) = engine.sync_once().await {
                tracing::warn!(folder = %folder_id, %error, "initial sync failed");
            }
        }
        Err(error) => {
            tracing::warn!(%error, "daemon control handoff failed");
        }
    }

    println!("watching folder {} at {}", folder_id, path.display());
    Ok(())
}

async fn folder_cmd(config_path: &Path, action: FolderAction) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let client = omni_transfer::Client::new(&config.server)?.with_token(config.token.clone());
    match action {
        FolderAction::Create { name } => {
            let created = client.create_folder(&name).await?;
            println!("{}", created.id);
        }
        FolderAction::List => {
            for folder in client.list_folders().await? {
                println!("{}  {}", folder.id, folder.name);
            }
        }
    }
    Ok(())
}

/// Resolve a local path for `add`: expand `~`, canonicalize, and offer to
/// create a missing directory when we can prompt.
fn prepare_path(path: &Path, interactive: bool) -> anyhow::Result<PathBuf> {
    let expanded = wizard::expand_tilde(&path.to_string_lossy());
    if expanded.exists() {
        let canonical = expanded
            .canonicalize()
            .with_context(|| format!("resolving {}", expanded.display()))?;
        if !canonical.is_dir() {
            bail!("{} is not a directory", canonical.display());
        }
        return Ok(canonical);
    }
    if interactive
        && wizard::confirm(
            &format!("{} does not exist; create it?", expanded.display()),
            true,
        )?
    {
        std::fs::create_dir_all(&expanded)
            .with_context(|| format!("creating {}", expanded.display()))?;
        return Ok(expanded.canonicalize()?);
    }
    bail!("{} does not exist", expanded.display())
}

/// `omni-sync update`: check GitHub Releases, verify against SHA256SUMS, install.
async fn update(check: bool, force: bool, to: Option<String>) -> anyhow::Result<()> {
    let binary = "omni-sync";
    let target = omni_update::target();
    let current = omni_update::current_version();
    println!("current: {current} ({target})");

    let plan = if force {
        Some(omni_update::plan(binary, target, to.as_deref()).await?)
    } else {
        omni_update::check(binary, target, to.as_deref()).await?
    };

    let Some(plan) = plan else {
        println!("up to date");
        return Ok(());
    };

    println!(
        "available: {} ({}), asset {}",
        plan.release.version, plan.release.tag, plan.asset_name
    );
    if check {
        return Ok(());
    }

    let destination = std::env::current_exe().context("locating the running binary")?;
    println!("downloading and verifying…");
    omni_update::apply(&plan, &destination).await?;
    println!(
        "updated {} -> {}; restart the daemon to use it",
        plan.current, plan.release.version
    );
    Ok(())
}

async fn folders(config_path: &Path) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let client = omni_transfer::Client::new(&config.server)?.with_token(config.token.clone());
    for folder in client.list_folders().await? {
        println!("{}  {}", folder.id, folder.name);
    }
    Ok(())
}

fn status(config_path: &Path, filter: Option<&str>) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let store = Store::open_agent(&agent_db_path(config_path))?;
    for entry in &config.folders {
        if let Some(target) = filter
            && entry.folder_id != target
            && entry.path.as_path() != Path::new(target)
        {
            continue;
        }
        let folder = entry.folder_id.clone();
        let synced = store.with(move |conn| omni_store::agent::manifest(conn, &folder))?;
        let folder = entry.folder_id.clone();
        let seq = store.with(move |conn| omni_store::agent::applied_seq(conn, &folder))?;
        let folder = entry.folder_id.clone();
        let held = store.with(move |conn| omni_store::agent::held_files(conn, &folder))?;
        println!(
            "{}  {}  mode={}  {} file(s) synced  {} held  applied_seq={}",
            entry.folder_id,
            entry.path.display(),
            entry.mode,
            synced.len(),
            held.len(),
            seq
        );
        for (path, reason) in held {
            println!("  held {} ({reason})", String::from_utf8_lossy(&path));
        }
    }
    Ok(())
}

async fn flush(config_path: &Path, folder: Option<&str>, path: Option<&str>) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    if config.folders.is_empty() {
        bail!("no folders configured — run `omni-sync add <folder-id> <path>` first");
    }

    // If a daemon is already running for this config, ask it to execute the
    // flush so we never run a second concurrent engine on the same database.
    if let Some(synced) = daemon::try_send_command(
        config_path,
        daemon::ControlCommand::Flush {
            folder: folder.map(str::to_owned),
            path: path.map(str::to_owned),
        },
    )
    .await?
    {
        println!("synced {synced} folder(s) via running daemon");
        return Ok(());
    }

    let store = Store::open_agent(&agent_db_path(config_path))?;
    let target_path = path.map(|p| path_bytes(Path::new(p)));
    let mut synced = 0usize;

    for entry in &config.folders {
        let matched = match folder {
            None => true,
            Some(filter) => entry.folder_id == filter || entry.path.as_path() == Path::new(filter),
        };
        if !matched {
            continue;
        }
        let cfg = engine_config(entry)?;
        let client = omni_transfer::Client::new(&config.server)?.with_token(config.token.clone());
        let engine = Engine::new(client, store.clone(), cfg);
        engine.flush_once(target_path.as_deref()).await?;
        synced += 1;
    }

    if synced == 0 {
        bail!(
            "folder '{}' is not in {}",
            folder.unwrap_or_default(),
            config_path.display()
        );
    }
    println!("synced {synced} folder(s)");
    Ok(())
}

async fn history(config_path: &Path, folder: &str, path: &str) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let client = omni_transfer::Client::new(&config.server)?.with_token(config.token.clone());
    let folder_id = resolve_folder_id(&config, &client, folder).await?;
    let raw_path = path_bytes(Path::new(path));
    let entries = client.history(&folder_id, &raw_path).await?;
    if entries.is_empty() {
        println!("no versions recorded for {path}");
        return Ok(());
    }
    for entry in entries {
        let marker = if entry.active { "*" } else { " " };
        let restored = entry
            .restored_from
            .map(|v| format!(" (restored from v{v})"))
            .unwrap_or_default();
        println!(
            "{marker} v{}  seq={}  {} B  device={}  {}{restored}",
            entry.version_id, entry.seq, entry.size, entry.device_id, entry.created_at
        );
    }
    Ok(())
}

async fn resolve(config_path: &Path, folder: &str, path: &str, version: i64) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let client = omni_transfer::Client::new(&config.server)?.with_token(config.token.clone());
    let folder_id = resolve_folder_id(&config, &client, folder).await?;
    let raw_path = path_bytes(Path::new(path));
    let response = client.resolve(&folder_id, &raw_path, version).await?;
    if let Some(applied) = response.applied.first() {
        println!(
            "restored {} to version {} -> new version {} (seq {})",
            path, version, applied.version_id, applied.seq
        );
    }
    // Pull the restored version locally if this device watches the folder.
    if config.folders.iter().any(|e| e.folder_id == folder_id) {
        let _ = flush(config_path, Some(&folder_id), Some(path)).await;
    }
    Ok(())
}

async fn resolve_folder_id(
    config: &ClientConfig,
    client: &omni_transfer::Client,
    query: &str,
) -> anyhow::Result<String> {
    if let Some(entry) = config
        .folders
        .iter()
        .find(|e| e.folder_id == query || e.path.as_path() == Path::new(query))
    {
        return Ok(entry.folder_id.clone());
    }
    let folders = client.list_folders().await?;
    match match_folder(&folders, query)? {
        Some(found) => Ok(found.id),
        None => bail!("unknown folder '{query}'"),
    }
}

async fn doctor(config_path: &Path) -> anyhow::Result<()> {
    println!("config:   {}", config_path.display());
    println!("  exists: {}", config_path.exists());
    let config = load_config(config_path)?;
    println!("server:   {}", config.server);
    println!("device:   {}", config.device_id);
    println!("folders:  {}", config.folders.len());
    match omni_transfer::Client::new(&config.server)?
        .with_token(config.token.clone())
        .list_folders()
        .await
    {
        Ok(folders) => println!("server:   reachable ({} folder(s))", folders.len()),
        Err(error) => println!("server:   UNREACHABLE ({error})"),
    }
    Ok(())
}

pub(crate) fn load_config(explicit: &Path) -> anyhow::Result<ClientConfig> {
    let text = std::fs::read_to_string(explicit).with_context(|| {
        format!(
            "reading {} (run `omni-sync enroll` first)",
            explicit.display()
        )
    })?;
    Ok(toml::from_str(&text)?)
}

fn save_config(path: &Path, config: &ClientConfig) -> anyhow::Result<()> {
    write_private(path, &toml::to_string_pretty(config)?)
}

fn client_config_path(explicit: Option<&Path>) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    match ProjectDirs::from("dev", "albruiz", "omni-sync") {
        Some(project) => project.config_dir().join("client.toml"),
        None => PathBuf::from("client.toml"),
    }
}

/// The client database lives next to the config, keyed by the config's name, so
/// two clients in one directory (as in tests and the e2e script) never share it.
pub(crate) fn agent_db_path(config_path: &Path) -> PathBuf {
    let stem = config_path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("client");
    let dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!("{stem}.agent.db"))
}

/// Write `contents` to `path` atomically with `0600` permissions via a
/// same-directory temporary file and rename, so a config watcher never reads a
/// truncated or half-written file.
#[cfg(unix)]
pub(crate) fn write_private(path: &Path, contents: &str) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = temp_sibling(path);
    {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
    }
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn write_private(path: &Path, contents: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = temp_sibling(path);
    std::fs::write(&tmp, contents)?;
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error.into());
    }
    Ok(())
}

fn temp_sibling(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp.{}", std::process::id()));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dto(id: &str, name: &str) -> FolderDto {
        FolderDto {
            id: id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn folder_matching_prefers_exact_id_then_unique_name() {
        let folders = vec![
            dto("f-1", "Docs"),
            dto("f-2", "Photos"),
            dto("f-3", "Dup"),
            dto("f-4", "Dup"),
        ];

        assert_eq!(
            match_folder(&folders, "f-2").unwrap(),
            Some(dto("f-2", "Photos"))
        );
        assert_eq!(
            match_folder(&folders, "Docs").unwrap(),
            Some(dto("f-1", "Docs"))
        );
        assert_eq!(match_folder(&folders, "Missing").unwrap(), None);
        assert!(match_folder(&folders, "Dup").is_err());
    }

    #[test]
    fn engine_config_validates_the_sync_mode() {
        let mut entry = WatchEntry {
            folder_id: "f1".into(),
            path: PathBuf::from("/tmp"),
            mode: "on_close".into(),
            hold_while_process: vec!["steam*".into()],
            quiet_timeout_secs: Some(30),
        };
        let cfg = engine_config(&entry).unwrap();
        assert_eq!(cfg.mode, SyncMode::OnClose);
        assert_eq!(cfg.hold_rule.process_globs, vec!["steam*".to_string()]);
        assert_eq!(cfg.hold_rule.quiet_timeout_secs, 30);

        entry.mode = "bogus".into();
        assert!(engine_config(&entry).is_err());
    }

    #[test]
    fn atomic_private_write_replaces_the_target_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("client.toml");
        write_private(&target, "first = 1\n").unwrap();
        write_private(&target, "second = 2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "second = 2\n");
    }
}
