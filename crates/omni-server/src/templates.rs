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
    /// The op-log head this folder is at; members lag behind it.
    pub head_seq: i64,
    pub members: Vec<MemberView>,
}

pub struct MemberView {
    pub device_name: String,
    pub root: String,
    pub mode: String,
    pub acked_seq: i64,
    pub lag: i64,
}

/// The public enrollment landing page (`GET /enroll`).
#[derive(Template)]
#[template(path = "enroll.html")]
pub struct Enroll {
    pub version: String,
    pub code: Option<String>,
    /// Where to download a client build, e.g.
    /// `https://github.com/<owner>/<repo>/releases/latest`.
    pub releases_url: String,
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
