//! The `omni-sync` client binary.
//!
//! `enroll` registers the device; `add` records a folder to watch; `daemon`
//! runs the watcher/sync loop for every recorded folder.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use clap::{Parser, Subcommand};
use directories::ProjectDirs;
use omni_agent::{Engine, EngineConfig};
use omni_store::Store;

/// How often a folder is rescanned and the op log polled.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

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
    /// Register this device with a server using a one-time pairing code.
    Enroll {
        #[arg(long)]
        server: String,
        #[arg(long)]
        code: String,
        /// A human label for this device.
        #[arg(long, default_value = "device")]
        name: String,
    },
    /// Watch a shared folder (by id) at a local path.
    Add {
        folder: String,
        path: PathBuf,
        #[arg(long, default_value = "live")]
        mode: String,
    },
    /// Run the watcher/sync daemon.
    Daemon,
    /// List folders this server knows about.
    Folders,
    /// Show per-folder sync state.
    Status,
    /// Print the resolved config and store status.
    Doctor,
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
        Command::Enroll { server, code, name } => enroll(&config_path, &server, &code, &name).await,
        Command::Add { folder, path, mode } => add(&config_path, &folder, &path, &mode),
        Command::Daemon => daemon(&config_path).await,
        Command::Folders => folders(&config_path).await,
        Command::Status => status(&config_path),
        Command::Doctor => doctor(&config_path).await,
        Command::History { .. } | Command::Resolve { .. } | Command::Flush { .. } => {
            println!("not implemented yet — it lands in M5 (see plan.md).");
            Ok(())
        }
    }
}

async fn enroll(config_path: &Path, server: &str, code: &str, name: &str) -> anyhow::Result<()> {
    let client = omni_transfer::Client::new(server)?;
    let response = client.enroll(name, code).await?;

    let config = ClientConfig {
        server: server.to_string(),
        device_id: response.device_id.clone(),
        token: response.token,
        folders: Vec::new(),
    };
    save_config(config_path, &config)?;
    println!("enrolled as device {}", config.device_id);
    println!("config written to {}", config_path.display());
    Ok(())
}

fn add(config_path: &Path, folder: &str, path: &Path, mode: &str) -> anyhow::Result<()> {
    let mut config = load_config(config_path)?;
    let path = path
        .canonicalize()
        .with_context(|| format!("resolving {}", path.display()))?;
    if !path.is_dir() {
        return Err(anyhow!("{} is not a directory", path.display()));
    }
    config.folders.retain(|entry| entry.folder_id != folder);
    config.folders.push(WatchEntry {
        folder_id: folder.to_string(),
        path: path.clone(),
        mode: mode.to_string(),
    });
    save_config(config_path, &config)?;
    println!("watching folder {} at {}", folder, path.display());
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
                interval: POLL_INTERVAL,
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
        "omni-sync daemon: {} folder(s), polling every {}s",
        config.folders.len(),
        POLL_INTERVAL.as_secs()
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
