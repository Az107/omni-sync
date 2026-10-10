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
}

#[derive(Template)]
#[template(path = "login.html")]
pub struct Login {
    pub version: String,
    pub error: Option<String>,
    pub next: String,
}
