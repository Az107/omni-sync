//! Server configuration.
//!
//! Precedence is flags > env > file > defaults, the layering `space-elevator`
//! uses. The config never holds a secret: the dashboard password is stored as an
//! Argon2 PHC hash, and device tokens are stored only as SHA-256.

use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

/// Everything that can go wrong loading or writing the config.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("serialize: {0}")]
    Serialize(#[from] toml::ser::Error),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    /// Bind address. Loopback by default; the reverse proxy is in front.
    pub addr: String,
    /// Chunk bytes and other large blobs.
    pub data_dir: PathBuf,
    /// The database and other private state.
    pub state_dir: PathBuf,
    /// Argon2 PHC string for the dashboard login. Empty means "no password set".
    pub dashboard_password_hash: String,
}

impl Default for Config {
    fn default() -> Self {
        let project = ProjectDirs::from("dev", "albruiz", "omni-sync");
        let data_dir = project
            .as_ref()
            .map(|p| p.data_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("omni-sync-data"));
        let state_dir = project
            .as_ref()
            .and_then(|p| p.state_dir().map(Path::to_path_buf))
            .unwrap_or_else(|| data_dir.clone());
        Self {
            // Loopback: TLS and the public hostname are Traefik's job, not ours.
            addr: "127.0.0.1:8787".into(),
            data_dir,
            state_dir,
            dashboard_password_hash: String::new(),
        }
    }
}

impl Config {
    /// The per-user config file path.
    pub fn default_path() -> PathBuf {
        match ProjectDirs::from("dev", "albruiz", "omni-sync") {
            Some(p) => p.config_dir().join("server.toml"),
            None => PathBuf::from("server.toml"),
        }
    }

    /// Load the file at `explicit` (or the default path), then apply env
    /// overrides. A missing file is not an error: defaults are a valid server.
    pub fn load(explicit: Option<&Path>) -> Result<Self, ConfigError> {
        let path = explicit
            .map(Path::to_path_buf)
            .unwrap_or_else(Self::default_path);
        let mut config = if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            toml::from_str(&text)?
        } else {
            Self::default()
        };
        if let Ok(addr) = std::env::var("OMNI_SYNC_ADDR")
            && !addr.is_empty()
        {
            config.addr = addr;
        }
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        write_private(path, &text)?;
        Ok(())
    }

    pub fn db_path(&self) -> PathBuf {
        self.state_dir.join("server.db")
    }

    pub fn chunks_dir(&self) -> PathBuf {
        self.data_dir.join("chunks")
    }
}

/// Write a file that only its owner can read (0600).
#[cfg(unix)]
fn write_private(path: &Path, contents: &str) -> Result<(), ConfigError> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
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
fn write_private(path: &Path, contents: &str) -> Result<(), ConfigError> {
    std::fs::write(path, contents)?;
    Ok(())
}
