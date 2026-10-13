//! Page templates.
//!
//! Server-rendered HTML, no SPA and no client build step. Askama escapes every
//! interpolated value, which is what keeps a synced filename from becoming
//! script on the dashboard. The pages extend `base.html` and share `_rail.html`
//! (the navigation) and `_folder.html` (a folder plus its sync rails).

use askama::Template;

#[derive(Template)]
#[template(path = "dashboard.html")]
pub struct Dashboard {
    pub version: String,
    pub active: String,
    pub banner_kind: String,
    pub banner_head: String,
    pub banner_sub: String,
    pub folders: Vec<FolderView>,
    pub devices: Vec<DeviceView>,
}

/// One device row.
pub struct DeviceView {
    pub name: String,
    pub id: String,
    /// `.dot-*` suffix: `ok`, `warn` or `bad`.
    pub state_kind: String,
    pub state_label: String,
    /// Human age of the last contact; empty when never seen.
    pub seen: String,
}

#[derive(Template)]
#[template(path = "devices.html")]
pub struct Devices {
    pub version: String,
    pub active: String,
    pub devices: Vec<DeviceView>,
    /// A freshly minted pairing code, shown once.
    pub code: Option<String>,
}

#[derive(Template)]
#[template(path = "folders.html")]
pub struct Folders {
    pub version: String,
    pub active: String,
    pub folders: Vec<FolderView>,
}

pub struct FolderView {
    pub id: String,
    pub name: String,
    /// `.dot-*` suffix for the folder's overall state.
    pub state_kind: String,
    pub state_label: String,
    pub members: Vec<MemberView>,
}

/// One device's hold on a folder, as the sync rail renders it.
pub struct MemberView {
    pub device_name: String,
    pub root: String,
    /// Ops this device has not yet acknowledged.
    pub lag: i64,
    /// Percentage of the log this device has acknowledged (0..=100).
    pub pct: i64,
    pub lag_label: String,
    /// `img` alt text for the rail.
    pub state_label: String,
}

#[derive(Template)]
#[template(path = "overwrites.html")]
pub struct Overwrites {
    pub version: String,
    pub active: String,
    pub rows: Vec<OverwriteView>,
}

pub struct OverwriteView {
    pub id: i64,
    pub folder: String,
    pub path: String,
    pub losing: VersionView,
    pub winning: VersionView,
}

pub struct VersionView {
    pub version_id: i64,
    pub device: String,
    pub when: String,
    pub size: String,
}

#[derive(Template)]
#[template(path = "login.html")]
pub struct Login {
    pub version: String,
    pub error: Option<String>,
    pub next: String,
}

/// The public enrollment landing page (`GET /enroll`).
#[derive(Template)]
#[template(path = "enroll.html")]
pub struct Enroll {
    pub version: String,
    pub code: Option<String>,
    /// Where to download a client build.
    pub releases_url: String,
}
