//! The `omni-sync` client binary.
//!
//! Only `enroll` is wired to a real server so far; the rest are placeholders
//! that name the milestone they land in. Enrollment is enough to prove the API
//! contract end to end: `enroll` → token on disk → device visible in the
//! dashboard.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use directories::ProjectDirs;

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
    /// Join a shared folder with a local path.
    Add {
        folder: String,
        path: PathBuf,
        #[arg(long, default_value = "live")]
        mode: String,
    },
    /// Run the watcher/sync daemon.
    Daemon,
    /// Show per-folder state, seq and holds.
    Status {
        #[arg(long)]
        folder: Option<String>,
    },
    /// List folders this device is a member of.
    Folders,
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
    /// Check the config, server reachability and store.
    Doctor,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ClientConfig {
    server: String,
    device_id: String,
    token: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Enroll { server, code, name } => {
            enroll(cli.config.as_deref(), &server, &code, &name).await
        }
        Command::Add { mode, .. } => {
            println!("`add` (mode={mode}) is not implemented yet — it lands in M3 (see plan.md).");
            Ok(())
        }
        Command::Daemon => {
            println!("`daemon` is not implemented yet — it lands in M3 (see plan.md).");
            Ok(())
        }
        Command::Status { .. } | Command::Folders | Command::Doctor => {
            println!("not implemented yet — it lands in M3 (see plan.md).");
            Ok(())
        }
        Command::History { .. } | Command::Resolve { .. } | Command::Flush { .. } => {
            println!("not implemented yet — it lands in M5 (see plan.md).");
            Ok(())
        }
    }
}

async fn enroll(
    explicit: Option<&std::path::Path>,
    server: &str,
    code: &str,
    name: &str,
) -> anyhow::Result<()> {
    let client = omni_transfer::Client::new(server)?;
    let response = client.enroll(name, code).await?;

    let config = ClientConfig {
        server: server.to_string(),
        device_id: response.device_id.clone(),
        token: response.token,
    };
    let path = client_config_path(explicit);
    write_private(&path, &toml::to_string_pretty(&config)?)?;

    println!("enrolled as device {}", config.device_id);
    println!("config written to {}", path.display());
    Ok(())
}

fn client_config_path(explicit: Option<&std::path::Path>) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    match ProjectDirs::from("dev", "albruiz", "omni-sync") {
        Some(project) => project.config_dir().join("client.toml"),
        None => PathBuf::from("client.toml"),
    }
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, contents: &str) -> anyhow::Result<()> {
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
fn write_private(path: &std::path::Path, contents: &str) -> anyhow::Result<()> {
    std::fs::write(path, contents)?;
    Ok(())
}
