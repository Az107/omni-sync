//! HTMX page templates.
//!
//! Server-rendered HTML, no SPA and no client build step. Askama escapes every
//! interpolated value, which is what keeps a synced filename from becoming
//! script on the dashboard.

use askama::Template;

#[derive(Template)]
#[template(path = "dashboard.html")]
pub struct Dashboard {
    pub version: String,
    pub devices: i64,
    pub folders: i64,
    pub overwrites: i64,
}

/// One row of the devices page.
pub struct DeviceView {
    pub name: String,
    pub id: String,
    pub last_seen: String,
    pub revoked: bool,
}

#[derive(Template)]
#[template(path = "devices.html")]
pub struct Devices {
    pub version: String,
    pub devices: Vec<DeviceView>,
    /// A freshly minted pairing code, shown once.
    pub code: Option<String>,
}

#[derive(Template)]
#[template(path = "folders.html")]
pub struct Folders {
    pub version: String,
    pub folders: Vec<FolderView>,
}

pub struct FolderView {
    pub id: String,
    pub name: String,
}

#[derive(Template)]
#[template(path = "overwrites.html")]
pub struct Overwrites {
    pub version: String,
    pub rows: Vec<OverwriteView>,
}

pub struct OverwriteView {
    pub id: i64,
    pub folder: String,
    pub path: String,
    pub losing_version: i64,
    pub winning_version: i64,
}

#[derive(Template)]
#[template(path = "login.html")]
pub struct Login {
    pub version: String,
    pub error: Option<String>,
    pub next: String,
}
