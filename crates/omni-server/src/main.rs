//! The `omni-sync-server` binary.

use std::path::PathBuf;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use omni_server::app;
use omni_server::auth;
use omni_server::config::Config;
use omni_server::state::AppState;
use omni_store::{Store, devices, folders, pairing};

#[derive(Parser)]
#[command(
    name = "omni-sync-server",
    version,
    about = "omni-sync server: file-sync API and dashboard"
)]
struct Cli {
    /// Path to the config file. Defaults to the per-user config directory.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write a starter config, generating a dashboard password if needed.
    Init,
    /// Run the API and the dashboard.
    Serve {
        /// Override the bind address (loopback by default).
        #[arg(long)]
        addr: Option<String>,
    },
    /// Mint a one-time pairing code for enrolling a device.
    Pair {
        /// How long the code stays valid, in seconds.
        #[arg(long, default_value_t = 600)]
        ttl: i64,
    },
    /// Manage shared folders from the command line.
    Folder {
        #[command(subcommand)]
        action: FolderAction,
    },
    /// Print the resolved config and database status.
    Doctor,
}

#[derive(Subcommand)]
enum FolderAction {
    /// Create a folder and print its id.
    Create { name: String },
    /// List folders (id and name).
    List,
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
    match cli.command {
        Command::Init => init(cli.config.as_deref()),
        Command::Serve { addr } => serve(cli.config.as_deref(), addr).await,
        Command::Pair { ttl } => pair(cli.config.as_deref(), ttl),
        Command::Folder { action } => folder(cli.config.as_deref(), action),
        Command::Doctor => doctor(cli.config.as_deref()),
    }
}

fn folder(explicit: Option<&std::path::Path>, action: FolderAction) -> anyhow::Result<()> {
    let config = Config::load(explicit)?;
    std::fs::create_dir_all(&config.state_dir)?;
    let store = Store::open(&config.db_path())
        .with_context(|| format!("opening {}", config.db_path().display()))?;
    match action {
        FolderAction::Create { name } => {
            let folder = folders::Folder {
                id: uuid::Uuid::now_v7().to_string(),
                name,
                retention_versions: 50,
                retention_days: 90,
                encrypted: false,
            };
            let id = folder.id.clone();
            store.with(|conn| folders::create(conn, &folder))?;
            println!("{id}");
        }
        FolderAction::List => {
            for folder in store.with(folders::list)? {
                println!("{}  {}", folder.id, folder.name);
            }
        }
    }
    Ok(())
}

fn pair(explicit: Option<&std::path::Path>, ttl: i64) -> anyhow::Result<()> {
    let config = Config::load(explicit)?;
    std::fs::create_dir_all(&config.state_dir)?;
    let store = Store::open(&config.db_path())
        .with_context(|| format!("opening {}", config.db_path().display()))?;
    let code = auth::new_pairing_code();
    store.with(|conn| pairing::create(conn, &code, ttl))?;
    println!("{code}");
    println!("valid for {ttl}s — enroll with:");
    println!("  omni-sync enroll --server <server-url> --code {code}");
    Ok(())
}

fn init(explicit: Option<&std::path::Path>) -> anyhow::Result<()> {
    let path = explicit
        .map(PathBuf::from)
        .unwrap_or_else(Config::default_path);
    let mut config = Config::load(Some(&path))?;

    let mut generated = None;
    if config.dashboard_password_hash.is_empty() {
        let from_env = std::env::var("OMNI_SYNC_PASSWORD")
            .ok()
            .filter(|password| !password.is_empty());
        let password = match from_env {
            Some(password) => password,
            None => {
                let password = auth::random_password();
                generated = Some(password.clone());
                password
            }
        };
        config.dashboard_password_hash =
            auth::hash_password(&password).map_err(|e| anyhow::anyhow!(e))?;
    }

    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("creating {}", config.data_dir.display()))?;
    std::fs::create_dir_all(&config.state_dir)
        .with_context(|| format!("creating {}", config.state_dir.display()))?;
    config.save(&path)?;

    println!("config:    {}", path.display());
    println!("data dir:  {}", config.data_dir.display());
    println!("state dir: {}", config.state_dir.display());
    if let Some(password) = generated {
        println!("dashboard password: {password}");
    }
    Ok(())
}

async fn serve(
    explicit: Option<&std::path::Path>,
    addr_flag: Option<String>,
) -> anyhow::Result<()> {
    let config = Config::load(explicit)?;
    let addr = addr_flag.unwrap_or_else(|| config.addr.clone());

    std::fs::create_dir_all(&config.data_dir)?;
    std::fs::create_dir_all(&config.state_dir)?;
    let store = Store::open(&config.db_path())
        .with_context(|| format!("opening {}", config.db_path().display()))?;
    let state = AppState::new(store, config);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, "omni-sync-server listening; TLS terminates at the proxy");
    axum::serve(listener, app::router(state)).await?;
    Ok(())
}

fn doctor(explicit: Option<&std::path::Path>) -> anyhow::Result<()> {
    let config = Config::load(explicit)?;
    let config_path = explicit
        .map(PathBuf::from)
        .unwrap_or_else(Config::default_path);
    println!("config:      {}", config_path.display());
    println!("  exists:    {}", config_path.exists());
    println!("addr:        {}", config.addr);
    println!("data dir:    {}", config.data_dir.display());
    println!("state dir:   {}", config.state_dir.display());
    println!(
        "password:    {}",
        if config.dashboard_password_hash.is_empty() {
            "not set"
        } else {
            "set"
        }
    );

    let db_path = config.db_path();
    println!("database:    {}", db_path.display());
    if db_path.exists() {
        let store = Store::open(&db_path)?;
        let devices = store.with(devices::count)?;
        println!("  devices:   {devices}");
    } else {
        println!("  not created yet (run `serve`)");
    }
    Ok(())
}
