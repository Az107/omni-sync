//! Self-update from GitHub Releases.
//!
//! The release workflow publishes, per target, a bare binary named
//! `omni-sync-<version>-<target>` (`.exe` on Windows) plus a `SHA256SUMS`
//! file. This crate finds the newest release, picks the asset for the target
//! this binary was built for, verifies it against `SHA256SUMS`, and installs
//! it in place.
//!
//! Trust is checksum-only: `SHA256SUMS` and the asset come from the same
//! release, so this trusts whoever can publish a release. A signature
//! (minisign/sigstore) is the natural next step and would slot in beside
//! [`verify`].

use std::path::Path;

use semver::Version;
use serde::Deserialize;

/// The asset name for this binary's target, e.g.
/// `omni-sync-0.2.0-aarch64-apple-darwin` or `...-x86_64-pc-windows-msvc.exe`.
pub fn asset_name(binary: &str, version: &str, target: &str) -> String {
    if target.contains("windows") {
        format!("{binary}-{version}-{target}.exe")
    } else {
        format!("{binary}-{version}-{target}")
    }
}

/// The target this crate was compiled for, as emitted by `build.rs`.
pub fn target() -> &'static str {
    env!("OMNI_TARGET")
}

/// The version of the running binary.
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION is written as semver")
}

/// `(owner, repo)` parsed from the workspace's `repository` URL.
pub fn repository_parts() -> (String, String) {
    let url = env!("CARGO_PKG_REPOSITORY");
    let trimmed = url.trim_end_matches('/').trim_end_matches(".git");
    let mut segments = trimmed.rsplit('/');
    let repo = segments.next().unwrap_or("omni-sync").to_string();
    let owner = segments.next().unwrap_or("Az107").to_string();
    (owner, repo)
}

/// Everything that can go wrong checking or applying an update.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed version {0:?}")]
    Version(String),
    #[error("no release found for this repository (is it public, or set OMNI_SYNC_GITHUB_TOKEN?)")]
    NoRelease,
    #[error("GitHub said {status}: {body}")]
    Status { status: u16, body: String },
    #[error("release {tag} has no build for {target} (asset {asset:?})")]
    NoAsset {
        tag: String,
        target: String,
        asset: String,
    },
    #[error("release {0} has no SHA256SUMS to verify against")]
    NoChecksums(String),
    #[error("checksum mismatch for {0} — refusing to install")]
    Checksum(String),
}

/// One installable release.
#[derive(Clone, Debug)]
pub struct Release {
    pub tag: String,
    pub version: Version,
    pub html_url: String,
    pub assets: Vec<Asset>,
}

#[derive(Clone, Debug)]
pub struct Asset {
    pub name: String,
    pub url: String,
}

#[derive(Deserialize)]
struct ReleaseJson {
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<AssetJson>,
}

#[derive(Deserialize)]
struct AssetJson {
    name: String,
    browser_download_url: String,
}

const USER_AGENT: &str = concat!("omni-sync-updater/", env!("CARGO_PKG_VERSION"));

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .unwrap_or_default()
}

fn token_from_env() -> Option<String> {
    std::env::var("OMNI_SYNC_GITHUB_TOKEN")
        .ok()
        .filter(|token| !token.is_empty())
}

/// Fetch the newest release (`tag = None`) or a specific tag.
pub async fn release(tag: Option<&str>) -> Result<Release, UpdateError> {
    let (owner, repo) = repository_parts();
    let url = match tag {
        Some(tag) => format!("https://api.github.com/repos/{owner}/{repo}/releases/tags/{tag}"),
        None => format!("https://api.github.com/repos/{owner}/{repo}/releases/latest"),
    };

    let mut request = http()
        .get(url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json");
    if let Some(token) = token_from_env() {
        request = request.bearer_auth(token);
    }
    let response = request.send().await?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(UpdateError::NoRelease);
    }
    if !status.is_success() {
        return Err(UpdateError::Status {
            status: status.as_u16(),
            body: response.text().await.unwrap_or_default(),
        });
    }
    let json: ReleaseJson = response.json().await?;
    let version = Version::parse(json.tag_name.trim_start_matches('v'))
        .map_err(|_| UpdateError::Version(json.tag_name.clone()))?;
    Ok(Release {
        tag: json.tag_name,
        version,
        html_url: json.html_url,
        assets: json
            .assets
            .into_iter()
            .map(|asset| Asset {
                name: asset.name,
                url: asset.browser_download_url,
            })
            .collect(),
    })
}

/// What an update would do, without downloading anything yet.
#[derive(Clone, Debug)]
pub struct Plan {
    pub current: Version,
    pub release: Release,
    pub asset: Asset,
    /// The bare-binary name, e.g. `omni-sync-0.2.0-x86_64-unknown-linux-gnu`.
    pub asset_name: String,
}

/// Find a newer release for `binary`, or `None` when already current.
///
/// `target` overrides the compile-time target (for tests). `requested` pins a
/// specific tag instead of `latest`.
pub async fn check(
    binary: &str,
    target: &str,
    requested: Option<&str>,
) -> Result<Option<Plan>, UpdateError> {
    let plan = plan(binary, target, requested).await?;
    if requested.is_none() && plan.release.version <= plan.current {
        return Ok(None);
    }
    Ok(Some(plan))
}

/// Build a plan for the newest (or a pinned) release regardless of whether it
/// is newer than the running binary — what `--force` re-installs.
pub async fn plan(
    binary: &str,
    target: &str,
    requested: Option<&str>,
) -> Result<Plan, UpdateError> {
    let current = current_version();
    let release = release(requested).await?;
    let name = asset_name(binary, &release.version.to_string(), target);
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .cloned()
        .ok_or_else(|| UpdateError::NoAsset {
            tag: release.tag.clone(),
            target: target.to_string(),
            asset: name.clone(),
        })?;
    Ok(Plan {
        current,
        release,
        asset,
        asset_name: name,
    })
}

/// Download, verify and install the plan's asset over `destination`.
///
/// The bytes are checked against the release's `SHA256SUMS` *before* anything
/// is written, so a truncated or swapped download never replaces a working
/// binary.
pub async fn apply(plan: &Plan, destination: &Path) -> Result<(), UpdateError> {
    let sums_asset = plan
        .release
        .assets
        .iter()
        .find(|asset| asset.name == "SHA256SUMS")
        .ok_or_else(|| UpdateError::NoChecksums(plan.release.tag.clone()))?;
    let sums_text = fetch(sums_asset).await?;
    let sums = parse_sums(&String::from_utf8_lossy(&sums_text));

    let bytes = fetch(&plan.asset).await?;
    verify(&plan.asset_name, &bytes, &sums)?;
    install(destination, &bytes)
}

async fn fetch(asset: &Asset) -> Result<Vec<u8>, UpdateError> {
    let mut request = http().get(&asset.url);
    if let Some(token) = token_from_env() {
        request = request.bearer_auth(token);
    }
    let response = request.send().await?;
    let status = response.status();
    if !status.is_success() {
        return Err(UpdateError::Status {
            status: status.as_u16(),
            body: response.text().await.unwrap_or_default(),
        });
    }
    Ok(response.bytes().await?.to_vec())
}

/// Parse `sha256sum` output: `<hex>  <name>` (or `*name` for binary mode).
pub fn parse_sums(text: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((hash, name)) = line.split_once(char::is_whitespace) {
            let name = name.trim().trim_start_matches('*');
            if !name.is_empty() {
                map.insert(name.to_string(), hash.trim().to_ascii_lowercase());
            }
        }
    }
    map
}

/// Verify `bytes` against the SHA-256 recorded for `name`.
pub fn verify(
    name: &str,
    bytes: &[u8],
    sums: &std::collections::HashMap<String, String>,
) -> Result<(), UpdateError> {
    use sha2::{Digest, Sha256};
    let expected = sums
        .get(name)
        .ok_or_else(|| UpdateError::Checksum(name.to_string()))?;
    let actual = hex::encode(Sha256::digest(bytes));
    if &actual != expected {
        return Err(UpdateError::Checksum(name.to_string()));
    }
    Ok(())
}

/// Replace `destination` with `bytes`, atomically where the platform allows.
///
/// A running process keeps the old inode after the unix rename, so installing
/// over a running binary is safe; the new one takes effect on the next start.
/// Windows cannot rename over a running executable, so the current file is
/// moved aside first (a stale `.old` is cleaned up on the next update).
pub fn install(destination: &Path, bytes: &[u8]) -> Result<(), UpdateError> {
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let filename = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("omni-sync");
    let staging = parent.join(format!(".{filename}.new"));

    std::fs::write(&staging, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))?;
    }

    #[cfg(windows)]
    {
        let old = parent.join(format!(".{filename}.old"));
        let _ = std::fs::remove_file(&old);
        std::fs::rename(destination, &old)?;
    }
    std::fs::rename(&staging, destination)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_names_follow_the_release_matrix() {
        assert_eq!(
            asset_name("omni-sync", "0.2.0", "aarch64-apple-darwin"),
            "omni-sync-0.2.0-aarch64-apple-darwin"
        );
        assert_eq!(
            asset_name("omni-sync", "0.2.0", "x86_64-pc-windows-msvc"),
            "omni-sync-0.2.0-x86_64-pc-windows-msvc.exe"
        );
    }

    #[test]
    fn sums_parse_both_modes_and_ignore_blank_lines() {
        let sums = parse_sums(
            "abc123  omni-sync-0.1.0-x86_64-unknown-linux-gnu\n\nDEF456 *omni-sync-0.1.0-x86_64-pc-windows-msvc.exe\n",
        );
        assert_eq!(
            sums.get("omni-sync-0.1.0-x86_64-unknown-linux-gnu")
                .unwrap(),
            "abc123"
        );
        // The `*` binary marker is stripped and hashes are lowercased.
        assert_eq!(
            sums.get("omni-sync-0.1.0-x86_64-pc-windows-msvc.exe")
                .unwrap(),
            "def456"
        );
    }

    #[test]
    fn verification_accepts_the_right_bytes_and_refuses_the_wrong_ones() {
        use sha2::{Digest, Sha256};
        let bytes = b"a tiny fake binary";
        let hash = hex::encode(Sha256::digest(bytes));
        let sums = parse_sums(&format!(
            "{hash}  omni-sync-0.1.0-x86_64-unknown-linux-gnu\n"
        ));
        assert!(verify("omni-sync-0.1.0-x86_64-unknown-linux-gnu", bytes, &sums).is_ok());
        assert!(
            verify(
                "omni-sync-0.1.0-x86_64-unknown-linux-gnu",
                b"tampered",
                &sums
            )
            .is_err()
        );
        assert!(verify("absent-asset", bytes, &sums).is_err());
    }

    #[test]
    fn repository_is_parsed_from_the_manifest() {
        let (owner, repo) = repository_parts();
        assert!(!owner.is_empty() && !repo.is_empty());
        assert!(!repo.ends_with(".git"));
    }

    #[test]
    fn install_replaces_a_file_and_marks_it_executable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("omni-sync");
        std::fs::write(&path, b"old").unwrap();
        install(&path, b"new bytes").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new bytes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "the binary must stay executable");
        }
    }
}
