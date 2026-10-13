//! The `omni-sync` client binary.
//!
//! `enroll` registers the device (by pairing code or magic link); `add` records
//! a folder to watch; `daemon` runs the watcher/sync loop for every recorded
//! folder; `update` installs a newer release from GitHub.

mod link;
mod wizard;

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use clap::{Parser, Subcommand};
use directories::ProjectDirs;
use omni_agent::{Engine, EngineConfig};
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
    /// Register this device. Prompt-free with --server/--code or --link.
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
    /// Watch a shared folder (by id) at a local path. Prompts if args are omitted.
    Add {
        /// Folder id from `omni-sync folders`; prompted for if omitted.
        folder: Option<String>,
        /// Local directory to watch; prompted for if omitted.
        path: Option<PathBuf>,
        #[arg(long)]
        mode: Option<String>,
    },
    /// Run the watcher/sync daemon.
    Daemon,
    /// List folders this server knows about.
    Folders,
    /// Show per-folder sync state.
    Status,
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
    Flush {
        folder: Option<String>,
        path: Option<String>,
    },
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct ClientConfig {
    server: String,
    device_id: String,
    token: String,
    #[serde(default)]
    folders: Vec<WatchEntry>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct WatchEntry {
    folder_id: String,
    path: PathBuf,
    #[serde(default = "default_mode")]
    mode: String,
}

fn default_mode() -> String {
    "live".to_string()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
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
        Command::Add { folder, path, mode } => add(&config_path, folder, path, mode).await,
        Command::Daemon => daemon(&config_path).await,
        Command::Folders => folders(&config_path).await,
        Command::Status => status(&config_path),
        Command::Doctor => doctor(&config_path).await,
        Command::Update { check, force, to } => update(check, force, to).await,
        Command::History { .. } | Command::Resolve { .. } | Command::Flush { .. } => {
            println!("not implemented yet — it lands in M5 (see plan.md).");
            Ok(())
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
            _ => bail!("--server and --code (or --link) are required when not on a terminal"),
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

async fn add(
    config_path: &Path,
    folder: Option<String>,
    path: Option<PathBuf>,
    mode: Option<String>,
) -> anyhow::Result<()> {
    let mut config = load_config(config_path)?;
    let client = omni_transfer::Client::new(&config.server)?.with_token(config.token.clone());

    // Which server folder?
    let folder_id = match folder {
        Some(folder) => folder,
        None if interactive() => {
            let folders = client.list_folders().await?;
            wizard::select_folder(&folders)?
        }
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

    let mode = match mode {
        Some(mode) => mode,
        None if interactive() => {
            if wizard::confirm("Sync in live mode? (no = on_close)", true)? {
                "live".to_string()
            } else {
                "on_close".to_string()
            }
        }
        None => "live".to_string(),
    };

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

    config.folders.retain(|entry| entry.folder_id != folder_id);
    config.folders.push(WatchEntry {
        folder_id: folder_id.clone(),
        path: path.clone(),
        mode: mode.clone(),
    });
    save_config(config_path, &config)?;

    // Record membership on the server (best effort: an older server may not
    // know this endpoint, which must not stop the local add).
    let root = path.as_os_str().as_encoded_bytes().to_vec();
    if let Err(error) = client.register_member(&folder_id, &root, "both").await {
        eprintln!("note: the server did not record this membership ({error})");
    }

    println!("watching folder {} at {}", folder_id, path.display());
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

async fn daemon(config_path: &Path) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    if config.folders.is_empty() {
        return Err(anyhow!(
            "no folders configured — run `omni-sync add <folder-id> <path>` first"
        ));
    }
    let store = Store::open_agent(&agent_db_path(config_path))?;

    for entry in config.folders.iter().cloned() {
        let client = omni_transfer::Client::new(&config.server)?.with_token(config.token.clone());
        let engine = Engine::new(
            client,
            store.clone(),
            EngineConfig {
                folder_id: entry.folder_id.clone(),
                root: entry.path.clone(),
            },
        );
        tracing::info!(folder = %entry.folder_id, path = %entry.path.display(), "syncing");
        tokio::spawn(async move {
            if let Err(error) = engine.run().await {
                tracing::error!(%error, "sync engine stopped");
            }
        });
    }

    println!(
        "omni-sync daemon: {} folder(s); watching for changes, polling the log every 2s",
        config.folders.len()
    );
    tokio::signal::ctrl_c().await?;
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

fn status(config_path: &Path) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let store = Store::open_agent(&agent_db_path(config_path))?;
    for entry in &config.folders {
        let folder = entry.folder_id.clone();
        let synced = store.with(move |conn| omni_store::agent::manifest(conn, &folder))?;
        let folder = entry.folder_id.clone();
        let seq = store.with(move |conn| omni_store::agent::applied_seq(conn, &folder))?;
        println!(
            "{}  {}  {} file(s) synced  applied_seq={}",
            entry.folder_id,
            entry.path.display(),
            synced.len(),
            seq
        );
    }
    Ok(())
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

fn load_config(explicit: &Path) -> anyhow::Result<ClientConfig> {
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
fn agent_db_path(config_path: &Path) -> PathBuf {
    let stem = config_path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("client");
    let dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!("{stem}.agent.db"))
}

#[cfg(unix)]
fn write_private(path: &Path, contents: &str) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, contents: &str) -> anyhow::Result<()> {
    std::fs::write(path, contents)?;
    Ok(())
}
