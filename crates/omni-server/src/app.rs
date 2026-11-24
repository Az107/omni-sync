//! The axum application: `/api/v1` for devices, plus the dashboard.

use std::convert::Infallible;

use askama::Template as _;
use axum::body::Bytes;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;

use omni_core::{Op, OpKind};
use omni_netproto::{
    AppliedOpDto, ChunkPutResponse, CreateFolderRequest, DeletedFileDto, DeletedFilesResponse,
    DeviceDto, EnrollRequest, EnrollResponse, FolderDto, FoldersResponse, HeadResponse,
    HealthResponse, HistoryEntryDto, HistoryResponse, LogEntryDto, ManifestEntryDto,
    ManifestResponse, MemberDto, MembersResponse, OpKindDto, OpsResponse, OverwriteDto,
    PROTOCOL_VERSION, RegisterMemberRequest, RenameDeviceRequest, ResolveRequest, SubmitOpsRequest,
    SubmitOpsResponse, UpdateFolderRequest,
};
use omni_store::sync::{self, ActiveFile, LogEntry, OverwriteRow};
use omni_store::{StoreError, devices, folders, members, pairing};

use crate::auth;
use crate::chunkstore::ChunkError;
use crate::state::AppState;
use crate::templates::{
    Dashboard, DeviceView, Devices, Enroll, FolderView, Folders, Login, MemberView, OverwriteView,
    Overwrites, VersionView,
};

/// The dashboard session cookie name.
pub const SESSION_COOKIE: &str = "omni_session";

/// Embedded dashboard assets. `static/` is compiled into the binary, so a
/// deploy is one file and there is no separate web root to keep in sync.
#[derive(rust_embed::RustEmbed)]
#[folder = "static/"]
struct Assets;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/", get(dashboard))
        .route("/login", get(login_form).post(login_submit))
        .route("/logout", post(logout))
        .route("/enroll", get(enroll_page))
        .route("/static/{*path}", get(static_asset))
        // Dashboard pages.
        .route("/devices", get(devices_page))
        .route("/devices/pair", post(devices_pair))
        .route("/folders", get(folders_page).post(folders_create))
        .route("/folders/{id}/settings", post(folder_settings_update))
        .route("/overwrites", get(overwrites_page))
        .route("/overwrites/{id}/resolve", post(overwrite_resolve))
        .route("/overwrites/{id}/dismiss", post(overwrite_dismiss))
        // Device enrollment and self-management.
        .route("/api/v1/devices/enroll", post(enroll))
        .route("/api/v1/devices/me", patch(rename_me).delete(revoke_me))
        // Folders.
        .route("/api/v1/folders", post(create_folder).get(list_folders))
        .route("/api/v1/folders/{id}", patch(update_folder).get(get_folder))
        .route("/api/v1/folders/{id}/manifest", get(manifest))
        .route(
            "/api/v1/folders/{id}/members",
            post(register_member).get(list_members),
        )
        .route("/api/v1/folders/{id}/ops", post(submit_ops).get(list_ops))
        .route("/api/v1/folders/{id}/ops/stream", get(stream_ops))
        .route("/api/v1/folders/{id}/head", get(get_folder_head))
        .route("/api/v1/folders/{id}/deleted", get(list_deleted))
        .route("/api/v1/folders/{id}/history", get(history))
        .route("/api/v1/folders/{id}/resolve", post(resolve))
        .route("/api/v1/overwrites", get(list_overwrites))
        .route("/api/v1/overwrites/{id}/dismiss", post(dismiss_overwrite))
        // Chunks.
        .route(
            "/api/v1/chunks/{hash}",
            put(put_chunk)
                .get(get_chunk)
                .layer(axum::extract::DefaultBodyLimit::max(
                    omni_netproto::MAX_CHUNK_BYTES,
                )),
        )
        .with_state(state)
        // CORS is permissive on purpose: the API authenticates with a bearer
        // token in a header, never a cookie, so no cross-origin page can act as
        // a logged-in user by riding ambient credentials (the dashboard cookie
        // is SameSite=Strict and is not exposed by this layer). That leaves
        // nothing to protect by origin allowlisting, while a browser/WebView
        // client (the Obsidian plugin) may be served from `app://`, `file://`
        // or a random dev port. Credentials stay off, so `*` is safe here.
        .layer(CorsLayer::permissive())
}

/// Everything a handler can fail with.
pub enum AppError {
    Store(StoreError),
    Template(askama::Error),
    BadRequest(String),
    /// The op is well-formed but references content that is not available yet
    /// (a chunk that has not been uploaded). The client can retry.
    Conflict(String),
    Unauthorized,
    NotFound,
    TooManyRequests,
    Internal(String),
    Chunk(ChunkError),
}

impl From<StoreError> for AppError {
    fn from(error: StoreError) -> Self {
        // A bad op is the client's fault, not a storage failure. Mapping these
        // explicitly is what keeps a rejected batch at 400/409 instead of 500.
        match error {
            StoreError::InvalidOp(message) => AppError::BadRequest(message),
            StoreError::MissingChunk { hash } => {
                AppError::Conflict(format!("chunk {hash} has not been uploaded"))
            }
            StoreError::Unsupported(message) => AppError::BadRequest(message),
            other => AppError::Store(other),
        }
    }
}

impl From<askama::Error> for AppError {
    fn from(error: askama::Error) -> Self {
        AppError::Template(error)
    }
}

impl From<ChunkError> for AppError {
    fn from(error: ChunkError) -> Self {
        match error {
            ChunkError::Mismatch { .. }
            | ChunkError::InvalidHash(_)
            | ChunkError::TooLarge { .. } => AppError::BadRequest(error.to_string()),
            ChunkError::Io(_) => AppError::Internal(error.to_string()),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            AppError::Conflict(message) => (StatusCode::CONFLICT, message),
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized".to_string()),
            AppError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            AppError::TooManyRequests => (
                StatusCode::TOO_MANY_REQUESTS,
                "too many attempts; try again shortly".to_string(),
            ),
            AppError::Store(error) => {
                tracing::error!(%error, "store error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "storage error".to_string(),
                )
            }
            AppError::Template(error) => {
                tracing::error!(%error, "template error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "template error".to_string(),
                )
            }
            AppError::Internal(error) => {
                tracing::error!(%error, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            }
            AppError::Chunk(error) => {
                tracing::error!(%error, "chunk error");
                (StatusCode::INTERNAL_SERVER_ERROR, "chunk error".to_string())
            }
        };
        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}

// ---- public routes ---------------------------------------------------------

async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

async fn static_asset(Path(path): Path<String>) -> Response {
    match Assets::get(&path) {
        Some(file) => {
            let content_type = match path.rsplit('.').next() {
                Some("css") => "text/css",
                Some("js") => "text/javascript",
                Some("svg") => "image/svg+xml",
                Some("woff2") => "font/woff2",
                Some("txt") => "text/plain; charset=utf-8",
                _ => "application/octet-stream",
            };
            (
                [
                    (header::CONTENT_TYPE, content_type),
                    // Assets are embedded in the binary and only change on a
                    // release, so a short shared cache is safe.
                    (header::CACHE_CONTROL, "public, max-age=3600"),
                ],
                file.data.into_owned(),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn dashboard(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if !session_ok(&state, &headers) {
        return Ok(Redirect::to("/login").into_response());
    }
    let folders = folder_views(&state).await?;
    let devices = device_views(&state).await?;
    let overwrites = state
        .store
        .blocking(|conn| sync::overwrites(conn, true))
        .await?
        .len() as i64;

    let behind: i64 = folders
        .iter()
        .map(|folder| {
            folder
                .members
                .iter()
                .filter(|member| member.lag > 0)
                .count() as i64
        })
        .sum();
    let online = devices
        .iter()
        .filter(|device| device.state_kind != "bad")
        .count() as i64;
    let (banner_kind, banner_head, banner_sub) = status_summary(
        overwrites,
        behind,
        online,
        folders.len() as i64,
        devices.len() as i64,
    );

    let page = Dashboard {
        version: env!("CARGO_PKG_VERSION").to_string(),
        active: "overview".to_string(),
        banner_kind,
        banner_head,
        banner_sub,
        folders,
        devices,
    };
    Ok(Html(page.render()?).into_response())
}

#[derive(Deserialize)]
struct CodeQuery {
    #[serde(default)]
    code: Option<String>,
}

async fn devices_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CodeQuery>,
) -> Result<Response, AppError> {
    if !session_ok(&state, &headers) {
        return Ok(Redirect::to("/login").into_response());
    }
    let devices = device_views(&state).await?;
    let page = Devices {
        version: env!("CARGO_PKG_VERSION").to_string(),
        active: "devices".to_string(),
        devices,
        code: query.code,
    };
    Ok(Html(page.render()?).into_response())
}

async fn devices_pair(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if !session_ok(&state, &headers) {
        return Ok(Redirect::to("/login").into_response());
    }
    let code = auth::new_pairing_code();
    let created = code.clone();
    state
        .store
        .blocking(move |conn| pairing::create(conn, &created, 600))
        .await?;
    Ok(Redirect::to(&format!("/devices?code={code}")).into_response())
}

async fn folders_page(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if !session_ok(&state, &headers) {
        return Ok(Redirect::to("/login").into_response());
    }
    let folders = folder_views(&state).await?;
    let page = Folders {
        version: env!("CARGO_PKG_VERSION").to_string(),
        active: "folders".to_string(),
        folders,
    };
    Ok(Html(page.render()?).into_response())
}

#[derive(Deserialize)]
struct CreateFolderForm {
    name: String,
}

async fn folders_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CreateFolderForm>,
) -> Result<Response, AppError> {
    if !session_ok(&state, &headers) {
        return Ok(Redirect::to("/login").into_response());
    }
    let name = form.name.trim();
    if !name.is_empty() {
        let folder = folders::Folder::new(uuid::Uuid::now_v7().to_string(), name.to_string());
        state
            .store
            .blocking(move |conn| folders::create(conn, &folder))
            .await?;
    }
    Ok(Redirect::to("/folders").into_response())
}

#[derive(Deserialize)]
struct FolderSettingsForm {
    #[serde(default)]
    exclude: String,
    /// Present only when the checkbox is ticked.
    #[serde(default)]
    preserve_permissions: Option<String>,
}

/// Save a folder's sync policy from the dashboard. The same fields are
/// editable through `PATCH /api/v1/folders/{id}`, which is what clients use.
async fn folder_settings_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(folder_id): Path<String>,
    Form(form): Form<FolderSettingsForm>,
) -> Result<Response, AppError> {
    if !session_ok(&state, &headers) {
        return Ok(Redirect::to("/login").into_response());
    }
    let exclude: Vec<String> = form
        .exclude
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    let preserve = form.preserve_permissions.is_some();
    let updated = state
        .store
        .blocking(move |conn| {
            folders::update_settings(conn, &folder_id, Some(&exclude), Some(preserve))
        })
        .await?;
    if !updated {
        return Err(AppError::NotFound);
    }
    Ok(Redirect::to("/folders").into_response())
}

async fn overwrites_page(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if !session_ok(&state, &headers) {
        return Ok(Redirect::to("/login").into_response());
    }
    let rows = state
        .store
        .blocking(|conn| sync::overwrites_detailed(conn, true))
        .await?;
    let folders = state.store.blocking(folders::list).await?;
    let names: std::collections::HashMap<String, String> = folders
        .into_iter()
        .map(|folder| (folder.id, folder.name))
        .collect();

    let page = Overwrites {
        version: env!("CARGO_PKG_VERSION").to_string(),
        active: "overwrites".to_string(),
        rows: rows
            .into_iter()
            .map(|row| OverwriteView {
                id: row.id,
                folder: names
                    .get(&row.folder_id)
                    .cloned()
                    .unwrap_or_else(|| row.folder_id.clone()),
                path: display_path(&row.path),
                losing: VersionView {
                    version_id: row.losing_version_id,
                    device: row.losing_device,
                    when: human_age(row.losing_age_secs),
                    size: human_size(row.losing_size),
                },
                winning: VersionView {
                    version_id: row.winning_version_id,
                    device: row.winning_device,
                    when: human_age(row.winning_age_secs),
                    size: human_size(row.winning_size),
                },
            })
            .collect(),
    };
    Ok(Html(page.render()?).into_response())
}

async fn overwrite_resolve(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    if !session_ok(&state, &headers) {
        return Ok(Redirect::to("/login").into_response());
    }
    let row = state
        .store
        .blocking(move |conn| sync::overwrite(conn, id))
        .await?
        .ok_or(AppError::NotFound)?;

    // Restore the superseded version's content at the same path.
    let losing = row.losing_version_id;
    let version = state
        .store
        .blocking(move |conn| sync::version(conn, losing))
        .await?
        .ok_or_else(|| AppError::BadRequest(format!("unknown version {losing}")))?;
    if version.folder_id != row.folder_id {
        return Err(AppError::BadRequest(format!(
            "version {losing} does not belong to this overwrite's folder"
        )));
    }

    let folder = row.folder_id.clone();
    let lookup = row.path.clone();
    let active = state
        .store
        .blocking(move |conn| sync::active_version_id(conn, &folder, &lookup))
        .await?;

    let op = Op {
        base_seq: 0,
        base_version_id: active,
        kind: OpKind::Resolve,
        path: row.path.clone(),
        from: None,
        size: version.size,
        mtime_ns: version.mtime_ns,
        mode: version.mode,
        chunks: version.chunks,
        restored_from: Some(version.id),
    };
    state.apply(&row.folder_id, "dashboard", op).await?;
    state.publish_ops(&row.folder_id);
    state
        .store
        .blocking(move |conn| sync::mark_overwrite_resolved(conn, id))
        .await?;
    Ok(Redirect::to("/overwrites").into_response())
}

async fn overwrite_dismiss(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    if !session_ok(&state, &headers) {
        return Ok(Redirect::to("/login").into_response());
    }
    state
        .store
        .blocking(move |conn| sync::mark_overwrite_resolved(conn, id))
        .await?;
    Ok(Redirect::to("/overwrites").into_response())
}

async fn login_form(headers: HeaderMap) -> Result<Html<String>, AppError> {
    Ok(Html(login_page(None, "/", &headers)?))
}

#[derive(Deserialize)]
struct LoginForm {
    password: String,
    #[serde(default)]
    next: String,
}

async fn login_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>,
    Form(form): Form<LoginForm>,
) -> Result<Response, AppError> {
    let key = format!(
        "login:{}",
        client_key(&state, &headers, peer.map(|peer| peer.0.0))
    );
    if !state.auth_allowed(&key) {
        return Err(AppError::TooManyRequests);
    }
    if state.config.dashboard_password_hash.is_empty() {
        return Ok(Html(login_page(
            Some("no dashboard password is set — run `omni-sync-server init`"),
            "/",
            &HeaderMap::new(),
        )?)
        .into_response());
    }

    // Argon2 is CPU-heavy and deliberately slow. Run it on the blocking pool,
    // with a permit bounding how many verifications compete for the runtime's
    // CPU, so a login flood cannot stall the sync API.
    let password = form.password;
    let phc = state.config.dashboard_password_hash.clone();
    let permit = state.hash_permit().ok_or(AppError::TooManyRequests)?;
    let verified = tokio::task::spawn_blocking(move || {
        // The blocking work survives request cancellation. Keep its permit
        // inside the closure so cancelled callers cannot bypass the bound.
        let _permit = permit;
        auth::verify_password(&password, &phc)
    })
    .await
    .map_err(|error| AppError::Internal(error.to_string()))?;

    if !verified {
        return Ok(Html(login_page(
            Some("incorrect password"),
            safe_next(&form.next),
            &HeaderMap::new(),
        )?)
        .into_response());
    }
    state.auth_reset(&key);

    let handle = state.create_session();
    let target = safe_next(&form.next);
    let mut response = Redirect::to(target).into_response();
    let mut cookie =
        format!("{SESSION_COOKIE}={handle}; HttpOnly; SameSite=Strict; Path=/; Max-Age=86400");
    if is_https(&state, &headers) {
        cookie.push_str("; Secure");
    }
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(|e| AppError::Internal(e.to_string()))?,
    );
    Ok(response)
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(handle) = cookie(&headers, SESSION_COOKIE) {
        state.drop_session(handle);
    }
    let mut response = Redirect::to("/login").into_response();
    let mut cookie = format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0");
    if is_https(&state, &headers) {
        cookie.push_str("; Secure");
    }
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

// ---- enrollment ------------------------------------------------------------

#[derive(Deserialize)]
struct EnrollQuery {
    #[serde(default)]
    code: Option<String>,
    /// A loopback callback the browser should be handed to, e.g.
    /// `http://127.0.0.1:49152`.
    #[serde(default)]
    cb: Option<String>,
    /// An opaque nonce the client checks, to defeat a random page POSTing a
    /// code to the loopback listener.
    #[serde(default)]
    state: Option<String>,
}

/// The public enrollment landing page (`GET /enroll`).
///
/// With `code` + a loopback `cb`, this is a plain redirect: the browser hands
/// the one-time code to the client's listener, which is the whole point of the
/// magic link. Otherwise it renders instructions. The pairing code *is* the
/// secret here, so there is no session check; the code is single-use and TTL'd.
async fn enroll_page(Query(query): Query<EnrollQuery>) -> Result<Response, AppError> {
    if let (Some(code), Some(cb)) = (&query.code, &query.cb)
        && is_loopback_callback(cb)
    {
        let separator = if cb.contains('?') { '&' } else { '?' };
        let mut target = format!("{cb}{separator}code={}", percent_encode(code));
        if let Some(state) = &query.state {
            target.push_str("&state=");
            target.push_str(&percent_encode(state));
        }
        return Ok(Redirect::to(&target).into_response());
    }

    let page = Enroll {
        version: env!("CARGO_PKG_VERSION").to_string(),
        code: query.code,
        releases_url: format!("{}/releases/latest", env!("CARGO_PKG_REPOSITORY")),
    };
    let mut response = Html(page.render()?).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'self'; font-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'",
        ),
    );
    Ok(response)
}

/// Only ever redirect to a loopback callback. A browser treats
/// `http://127.0.0.1` and `http://localhost` as trustworthy, so this is the
/// one host an HTTPS page may hand off to; anything else would be an open
/// redirect.
///
/// This parses the URL instead of prefix-matching it. Prefix matching is how
/// `http://127.0.0.1:1234@evil.example/` slips through: everything up to the
/// `@` looks like the loopback prefix, but the browser actually navigates to
/// `evil.example`. The accepted shape is deliberately narrow — plain `http`,
/// one of the literal loopback hosts, an explicit numeric port, no credentials
/// in the authority, and no fragment. A path or query on that loopback origin
/// is still loopback and is allowed.
fn is_loopback_callback(cb: &str) -> bool {
    if cb
        .bytes()
        .any(|b| b < 0x20 || b == 0x7f || b == b' ' || b == b'\\')
    {
        return false;
    }
    let Ok(url) = url::Url::parse(cb) else {
        return false;
    };
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
    {
        return false;
    }
    let Some(authority_and_path) = cb.strip_prefix("http://") else {
        return false;
    };
    // A fragment is never sent to the listener and would swallow the appended
    // `code`; credentials in the authority are the userinfo attack. A path or
    // query on the loopback origin is harmless and allowed.
    if authority_and_path.contains('#') {
        return false;
    }
    let authority_end = authority_and_path
        .find(['/', '?'])
        .unwrap_or(authority_and_path.len());
    let authority = &authority_and_path[..authority_end];
    if authority.contains('@') {
        return false;
    }

    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let Some(end) = rest.find(']') else {
            return false;
        };
        if &rest[..end] != "::1" {
            return false;
        }
        let Some(port) = rest[end + 1..].strip_prefix(':') else {
            return false;
        };
        ("::1", port)
    } else {
        let Some((host, port)) = authority.rsplit_once(':') else {
            return false;
        };
        if host.contains(':') {
            return false;
        }
        (host, port)
    };

    let host_is_loopback =
        host == "127.0.0.1" || host == "::1" || host.eq_ignore_ascii_case("localhost");
    host_is_loopback && valid_port(port)
}

/// A decimal TCP port in `1..=65535`, with no sign, whitespace or empty value.
fn valid_port(port: &str) -> bool {
    !port.is_empty()
        && port.len() <= 5
        && port.bytes().all(|b| b.is_ascii_digit())
        && port.parse::<u16>().map(|port| port != 0).unwrap_or(false)
}

/// Percent-encode everything outside the RFC 3986 unreserved set. Used for the
/// code and nonce in a redirect target, where an unescaped `&` or CRLF would be
/// an injection.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

async fn enroll(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>,
    Json(request): Json<EnrollRequest>,
) -> Result<Json<EnrollResponse>, AppError> {
    let key = format!(
        "enroll:{}",
        client_key(&state, &headers, peer.map(|peer| peer.0.0))
    );
    if !state.auth_allowed(&key) {
        return Err(AppError::TooManyRequests);
    }
    if request.protocol_version != PROTOCOL_VERSION {
        return Err(AppError::BadRequest(format!(
            "unsupported protocol version {} (server speaks {PROTOCOL_VERSION})",
            request.protocol_version
        )));
    }
    if request.code.trim().is_empty() {
        return Err(AppError::BadRequest("a pairing code is required".into()));
    }

    let device_id = uuid::Uuid::now_v7().to_string();
    let token = auth::new_device_token();
    let device = devices::Device {
        id: device_id.clone(),
        name: request.name,
        pubkey: Vec::new(),
        token_hash: auth::hash_token(&token),
        revoked: false,
        last_seen_at: None,
    };
    let code = request.code;

    let consumed = state
        .store
        .blocking_mut(move |conn| {
            if !pairing::consume(conn, &code, &device.id)? {
                return Ok(false);
            }
            devices::insert(conn, &device)?;
            Ok(true)
        })
        .await?;

    if !consumed {
        return Err(AppError::BadRequest(
            "invalid, expired or already-used pairing code".into(),
        ));
    }
    state.auth_reset(&key);
    Ok(Json(EnrollResponse { device_id, token }))
}

/// `PATCH /api/v1/devices/me` — the calling device renames itself.
async fn rename_me(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<RenameDeviceRequest>,
) -> Result<Json<DeviceDto>, AppError> {
    let device = authenticate(&state, &headers).await?;
    let name = request.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("a device name is required".into()));
    }
    let id = device.id.clone();
    let new_name = name.to_string();
    let renamed = state
        .store
        .blocking(move |conn| devices::rename(conn, &id, &new_name))
        .await?;
    if !renamed {
        return Err(AppError::NotFound);
    }
    Ok(Json(DeviceDto {
        id: device.id,
        name: name.to_string(),
    }))
}

/// `DELETE /api/v1/devices/me` — the calling device revokes its own token. The
/// request authenticates with that token first, so a device cannot revoke
/// another one.
async fn revoke_me(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<StatusCode, AppError> {
    let device = authenticate(&state, &headers).await?;
    let id = device.id;
    let revoked = state
        .store
        .blocking(move |conn| devices::revoke(conn, &id))
        .await?;
    if !revoked {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---- auth helper -----------------------------------------------------------

async fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<devices::Device, AppError> {
    let token = bearer(headers).ok_or(AppError::Unauthorized)?.to_string();
    let hash = auth::hash_token(&token);
    let device = state
        .store
        .blocking(move |conn| devices::by_token_hash(conn, &hash))
        .await?
        .filter(|device| !device.revoked)
        .ok_or(AppError::Unauthorized)?;
    let device_id = device.id.clone();
    // Best-effort presence update; a failure must not fail the request.
    let _ = state
        .store
        .blocking(move |conn| devices::touch(conn, &device_id))
        .await;
    Ok(device)
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

// ---- folders ---------------------------------------------------------------

async fn create_folder(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateFolderRequest>,
) -> Result<Json<FolderDto>, AppError> {
    let _device = authenticate(&state, &headers).await?;
    let name = request.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("a folder name is required".into()));
    }
    if request.exclude.iter().any(|pattern| pattern.contains('\0')) {
        return Err(AppError::BadRequest(
            "an exclude pattern cannot contain a NUL".into(),
        ));
    }
    let mut folder = folders::Folder::new(uuid::Uuid::now_v7().to_string(), name.to_string());
    folder.exclude = request.exclude;
    folder.preserve_permissions = request.preserve_permissions;
    let created = folder.clone();
    state
        .store
        .blocking(move |conn| folders::create(conn, &created))
        .await?;
    Ok(Json(folder_dto(&folder)))
}

/// Change a folder's sync policy. Excludes and the permission policy are folder
/// properties, not device ones, so every client sees the same rule.
async fn update_folder(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(folder_id): Path<String>,
    Json(request): Json<UpdateFolderRequest>,
) -> Result<Json<FolderDto>, AppError> {
    let _device = authenticate(&state, &headers).await?;
    if let Some(patterns) = &request.exclude
        && patterns.iter().any(|pattern| pattern.contains('\0'))
    {
        return Err(AppError::BadRequest(
            "an exclude pattern cannot contain a NUL".into(),
        ));
    }
    let exclude = request.exclude;
    let preserve = request.preserve_permissions;
    let id = folder_id.clone();
    let updated = state
        .store
        .blocking(move |conn| {
            if !folders::update_settings(conn, &id, exclude.as_deref(), preserve)? {
                return Ok(None);
            }
            folders::get(conn, &id)
        })
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(folder_dto(&updated)))
}

async fn list_folders(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<FoldersResponse>, AppError> {
    let _device = authenticate(&state, &headers).await?;
    let folders = state.store.blocking(folders::list).await?;
    Ok(Json(FoldersResponse {
        folders: folders.iter().map(folder_dto).collect(),
    }))
}

async fn get_folder(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(folder_id): Path<String>,
) -> Result<Json<FolderDto>, AppError> {
    let _device = authenticate(&state, &headers).await?;
    let folder = state
        .store
        .blocking(move |conn| folders::get(conn, &folder_id))
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(folder_dto(&folder)))
}

/// A device registers (or moves) its root for a folder.
async fn register_member(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(folder_id): Path<String>,
    Json(request): Json<RegisterMemberRequest>,
) -> Result<Json<MemberDto>, AppError> {
    let device = authenticate(&state, &headers).await?;
    let root_path = omni_netproto::decode_path(&request.root_path)
        .map_err(|_| AppError::BadRequest("root_path is not valid base64".into()))?;

    let folder = folder_id.clone();
    let device_id = device.id.clone();
    let mode = request.mode;
    let member = state
        .store
        .blocking(move |conn| {
            if !folders::exists(conn, &folder)? {
                return Ok(None);
            }
            members::upsert(conn, &folder, &device_id, &root_path, &mode)?;
            Ok(Some(members::list(conn, &folder)?))
        })
        .await?
        .ok_or(AppError::NotFound)?
        .into_iter()
        .find(|member| member.device_id == device.id)
        .ok_or_else(|| AppError::Internal("member vanished after upsert".into()))?;
    Ok(Json(member_dto(member)))
}

async fn list_members(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(folder_id): Path<String>,
) -> Result<Json<MembersResponse>, AppError> {
    let _device = authenticate(&state, &headers).await?;
    let members = state
        .store
        .blocking(move |conn| members::list(conn, &folder_id))
        .await?;
    Ok(Json(MembersResponse {
        members: members.into_iter().map(member_dto).collect(),
    }))
}

async fn manifest(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(folder_id): Path<String>,
) -> Result<Json<ManifestResponse>, AppError> {
    let _device = authenticate(&state, &headers).await?;
    let files = state
        .store
        .blocking(move |conn| sync::manifest(conn, &folder_id))
        .await?;
    Ok(Json(ManifestResponse {
        entries: files.into_iter().map(entry_dto).collect(),
    }))
}

async fn submit_ops(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(folder_id): Path<String>,
    Json(request): Json<SubmitOpsRequest>,
) -> Result<Json<SubmitOpsResponse>, AppError> {
    let device = authenticate(&state, &headers).await?;

    // Decode and validate the whole batch before committing any of it. If one
    // op is malformed, the batch is refused and no prefix is applied, so a
    // client retry cannot double-apply the earlier ops.
    let ops = request
        .ops
        .iter()
        .map(|dto| {
            dto.to_core()
                .map_err(|_| AppError::BadRequest("a path is not valid base64".into()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    for op in &ops {
        omni_store::sync::validate_op(op)?;
    }

    let lookup = folder_id.clone();
    if !state
        .store
        .blocking(move |conn| folders::exists(conn, &lookup))
        .await?
    {
        return Err(AppError::NotFound);
    }

    let results = state.apply_batch(&folder_id, &device.id, &ops).await?;

    let mut overwrites = Vec::new();
    let mut applied = Vec::with_capacity(results.len());
    for (dto, result) in request.ops.iter().zip(results) {
        applied.push(AppliedOpDto {
            seq: result.seq,
            version_id: result.version_id,
        });
        if let Some(overwrite) = result.overwrite {
            overwrites.push(OverwriteDto {
                id: None,
                path: dto.path.clone(),
                losing_version_id: overwrite.losing_version_id,
                winning_version_id: overwrite.winning_version_id,
                folder_id: Some(folder_id.clone()),
            });
        }
    }

    // Wake any streaming client: its op log just grew.
    if !applied.is_empty() {
        state.publish_ops(&folder_id);
    }

    Ok(Json(SubmitOpsResponse {
        applied,
        overwrites,
    }))
}

/// `GET /api/v1/folders/{id}/ops/stream` — the op log as server-sent events.
///
/// Every event carries the folder's current head seq. The client treats it as a
/// wake-up ("something changed") and fetches the ops themselves through the
/// ordinary `ops?since=` endpoint, so the diff logic stays in one place and the
/// event payload stays tiny. A keep-alive comment flows on the default interval
/// so a connection that died in the middle is noticed by both ends.
async fn stream_ops(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(folder_id): Path<String>,
) -> Result<Response, AppError> {
    let _device = authenticate(&state, &headers).await?;

    // Subscribe *before* reading the head: a change landing in between is then
    // delivered as an event rather than missed between the two steps.
    let mut events = state.subscribe_ops();
    let head = folder_head(&state, &folder_id).await?;

    let stream = async_stream::stream! {
        yield Ok::<_, Infallible>(head_event(head));
        loop {
            let next = match events.recv().await {
                Ok(changed) if changed == folder_id => folder_head(&state, &folder_id).await,
                // Another folder changed; not ours to report.
                Ok(_) => continue,
                // We fell behind the broadcast buffer. The head still tells the
                // client to resync, so a lag is not fatal.
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    folder_head(&state, &folder_id).await
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };
            match next {
                Ok(head) => yield Ok(head_event(head)),
                Err(error) => {
                    tracing::error!(%error, "op stream stopped");
                    break;
                }
            }
        }
    };

    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}

/// An SSE event carrying a folder's op-log head.
fn head_event(head: i64) -> Event {
    Event::default().event("head").data(head.to_string())
}

/// The current op-log head of a folder.
async fn folder_head(state: &AppState, folder_id: &str) -> Result<i64, StoreError> {
    let id = folder_id.to_string();
    state
        .store
        .blocking(move |conn| sync::folder_head(conn, &id))
        .await
}

#[derive(Deserialize)]
struct SinceQuery {
    #[serde(default)]
    since: i64,
}

/// The op-log page size a `limit` is clamped into. The cap keeps one request
/// from loading a whole large folder's history into memory; a client that hits
/// it just pulls again with the returned `next`.
const MAX_OPS_PAGE: usize = 2000;

#[derive(Deserialize)]
struct OpsQuery {
    #[serde(default)]
    since: i64,
    /// Absent means "the whole tail", which is what the Rust client asks for.
    #[serde(default)]
    limit: Option<usize>,
}

async fn list_ops(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(folder_id): Path<String>,
    Query(query): Query<OpsQuery>,
) -> Result<Json<OpsResponse>, AppError> {
    let device = authenticate(&state, &headers).await?;
    let since = query.since;
    // A limit below 1 is treated as 1 rather than rejected: asking for "at
    // least one op" is the only sensible reading of `limit=0`.
    let limit = query.limit.map(|limit| limit.clamp(1, MAX_OPS_PAGE));
    let lookup = folder_id.clone();
    let (entries, has_more) = state
        .store
        .blocking(move |conn| sync::log_since_limited(conn, &lookup, since, limit))
        .await?;

    // A pull also tells us how far this device has caught up: it will apply
    // everything returned, so its watermark becomes the last seq (or its
    // `since` when nothing is new). This is what the dashboard's rail shows.
    let next = entries.last().map(|entry| entry.seq).unwrap_or(since);
    let folder = folder_id.clone();
    let device_id = device.id.clone();
    state
        .store
        .blocking(move |conn| members::set_acked(conn, &folder, &device_id, next))
        .await?;

    Ok(Json(OpsResponse {
        entries: entries.into_iter().map(log_dto).collect(),
        next,
        has_more,
    }))
}

/// `GET /api/v1/folders/{id}/head` — the newest op id, 0 when the log is empty.
/// The paginated client polls this cheaply instead of fetching a whole page.
async fn get_folder_head(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(folder_id): Path<String>,
) -> Result<Json<HeadResponse>, AppError> {
    let _device = authenticate(&state, &headers).await?;
    let head = folder_head(&state, &folder_id).await?;
    Ok(Json(HeadResponse { head }))
}

/// `GET /api/v1/folders/{id}/deleted` — the trash: every path whose file row is
/// marked deleted, newest first, each with the last content version before the
/// delete so the client can restore it through `/resolve`.
async fn list_deleted(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(folder_id): Path<String>,
) -> Result<Json<DeletedFilesResponse>, AppError> {
    let _device = authenticate(&state, &headers).await?;
    let files = state
        .store
        .blocking(move |conn| sync::deleted_files(conn, &folder_id))
        .await?;
    Ok(Json(DeletedFilesResponse {
        files: files
            .into_iter()
            .map(|file| DeletedFileDto {
                path: omni_netproto::encode_path(&file.path),
                version_id: file.version_id,
                size: file.size,
                deleted_at: file.deleted_at,
            })
            .collect(),
    }))
}

async fn list_overwrites(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<SinceQuery>,
) -> Result<Json<Vec<OverwriteDto>>, AppError> {
    let _device = authenticate(&state, &headers).await?;
    let since = query.since;
    let rows = state
        .store
        .blocking(move |conn| sync::overwrites_since(conn, true, since))
        .await?;
    Ok(Json(rows.into_iter().map(overwrite_dto).collect()))
}

/// Mark an overwrite review row as "keep the active version" without touching
/// any content. The dashboard has the same action as a form post.
async fn dismiss_overwrite(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    let _device = authenticate(&state, &headers).await?;
    state
        .store
        .blocking(move |conn| sync::mark_overwrite_resolved(conn, id))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct HistoryQuery {
    path: String,
}

async fn history(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(folder_id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<HistoryResponse>, AppError> {
    let _device = authenticate(&state, &headers).await?;
    let path = omni_netproto::decode_path(&query.path).unwrap_or_else(|_| query.path.into_bytes());
    let entries = state
        .store
        .blocking(move |conn| sync::history(conn, &folder_id, &path))
        .await?;
    Ok(Json(HistoryResponse {
        entries: entries
            .into_iter()
            .map(|entry| HistoryEntryDto {
                version_id: entry.version_id,
                seq: entry.seq,
                device_id: entry.device_id,
                base_version_id: entry.base_version_id,
                size: entry.size,
                mtime_ns: entry.mtime_ns,
                mode: entry.mode,
                restored_from: entry.restored_from,
                created_at: entry.created_at,
                active: entry.active,
            })
            .collect(),
    }))
}

async fn resolve(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(folder_id): Path<String>,
    Json(request): Json<ResolveRequest>,
) -> Result<Json<SubmitOpsResponse>, AppError> {
    let device = authenticate(&state, &headers).await?;
    let path = omni_netproto::decode_path(&request.path)
        .map_err(|_| AppError::BadRequest("bad path".into()))?;
    omni_store::sync::validate_path(&path)?;

    let lookup = folder_id.clone();
    if !state
        .store
        .blocking(move |conn| folders::exists(conn, &lookup))
        .await?
    {
        return Err(AppError::NotFound);
    }

    let version_id = request.restored_from;
    let version = state
        .store
        .blocking(move |conn| sync::version(conn, version_id))
        .await?
        .ok_or_else(|| AppError::BadRequest(format!("unknown version {version_id}")))?;
    if version.folder_id != folder_id {
        return Err(AppError::BadRequest(format!(
            "version {version_id} does not belong to this folder"
        )));
    }

    // The base is the active version of the path being *written*, not of the
    // path the restored content came from: restoring `b` from a version of `a`
    // must be compared against `b`'s active version to detect an overwrite.
    let folder = folder_id.clone();
    let lookup_path = path.clone();
    let active = state
        .store
        .blocking(move |conn| sync::active_version_id(conn, &folder, &lookup_path))
        .await?;

    // Restore the content under the requested path.
    let resolved_path = path.clone();
    let op = Op {
        base_seq: 0,
        base_version_id: active,
        kind: OpKind::Resolve,
        path,
        from: None,
        size: version.size,
        mtime_ns: version.mtime_ns,
        mode: version.mode,
        chunks: version.chunks,
        restored_from: Some(version.id),
    };
    let applied = state.apply(&folder_id, &device.id, op).await?;
    let folder = folder_id.clone();
    state
        .store
        .blocking(move |conn| sync::mark_path_overwrites_resolved(conn, &folder, &resolved_path))
        .await?;
    state.publish_ops(&folder_id);
    Ok(Json(SubmitOpsResponse {
        applied: vec![AppliedOpDto {
            seq: applied.seq,
            version_id: applied.version_id,
        }],
        overwrites: Vec::new(),
    }))
}

// ---- chunks ----------------------------------------------------------------

async fn put_chunk(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(hash): Path<String>,
    body: Bytes,
) -> Result<Json<ChunkPutResponse>, AppError> {
    let _device = authenticate(&state, &headers).await?;
    let size = body.len() as u64;
    if size > omni_netproto::MAX_CHUNK_BYTES as u64 {
        return Err(AppError::BadRequest(format!(
            "chunk is larger than the {}-byte limit",
            omni_netproto::MAX_CHUNK_BYTES
        )));
    }
    let chunks = state.chunks.clone();
    let write_hash = hash.clone();
    let created = tokio::task::spawn_blocking(move || chunks.put(&write_hash, &body))
        .await
        .map_err(|error| AppError::Internal(error.to_string()))??;
    let row_hash = hash.clone();
    state
        .store
        .blocking(move |conn| sync::ensure_chunk_row(conn, &row_hash, size))
        .await?;
    Ok(Json(ChunkPutResponse { created }))
}

async fn get_chunk(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(hash): Path<String>,
) -> Result<Response, AppError> {
    let _device = authenticate(&state, &headers).await?;
    let chunks = state.chunks.clone();
    let bytes = tokio::task::spawn_blocking(move || chunks.get(&hash))
        .await
        .map_err(|error| AppError::Internal(error.to_string()))??;
    match bytes {
        Some(bytes) => {
            Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
        }
        None => Err(AppError::NotFound),
    }
}

// ---- DTO conversions -------------------------------------------------------

fn folder_dto(folder: &folders::Folder) -> FolderDto {
    FolderDto {
        id: folder.id.clone(),
        name: folder.name.clone(),
        exclude: folder.exclude.clone(),
        preserve_permissions: folder.preserve_permissions,
    }
}

fn member_dto(member: members::Member) -> MemberDto {
    MemberDto {
        device_id: member.device_id,
        device_name: member.device_name,
        root_path: omni_netproto::encode_path(&member.root_path),
        mode: member.mode,
        acked_seq: member.acked_seq,
    }
}

fn entry_dto(file: ActiveFile) -> ManifestEntryDto {
    ManifestEntryDto {
        path: omni_netproto::encode_path(&file.path),
        version_id: file.version_id,
        size: file.size,
        mtime_ns: file.mtime_ns,
        mode: file.mode,
        chunks: omni_netproto::encode_chunks(&file.chunks),
    }
}

fn log_dto(entry: LogEntry) -> LogEntryDto {
    LogEntryDto {
        seq: entry.seq,
        version_id: entry.version_id,
        kind: kind_dto(&entry.kind),
        path: omni_netproto::encode_path(&entry.path),
        device_id: entry.device_id,
        size: entry.size,
        mtime_ns: entry.mtime_ns,
        mode: entry.mode,
        chunks: omni_netproto::encode_chunks(&entry.chunks),
    }
}

fn kind_dto(kind: &str) -> OpKindDto {
    match kind {
        "delete" => OpKindDto::Delete,
        "move" => OpKindDto::Move,
        "resolve" => OpKindDto::Resolve,
        _ => OpKindDto::Put,
    }
}

fn overwrite_dto(row: OverwriteRow) -> OverwriteDto {
    OverwriteDto {
        id: Some(row.id),
        path: omni_netproto::encode_path(&row.path),
        losing_version_id: row.losing_version_id,
        winning_version_id: row.winning_version_id,
        folder_id: Some(row.folder_id),
    }
}

// ---- helpers ---------------------------------------------------------------

/// Every folder with its members, each carrying the data the sync rail needs.
async fn folder_views(state: &AppState) -> Result<Vec<FolderView>, AppError> {
    let folders = state.store.blocking(folders::list).await?;
    let mut views = Vec::with_capacity(folders.len());
    for folder in folders {
        let id = folder.id.clone();
        let head = state
            .store
            .blocking(move |conn| sync::folder_head(conn, &id))
            .await?;
        let lookup = folder.id.clone();
        let members = state
            .store
            .blocking(move |conn| members::list(conn, &lookup))
            .await?;

        let member_views: Vec<MemberView> = members
            .into_iter()
            .map(|member| member_view(member, head))
            .collect();
        let behind = member_views.iter().filter(|member| member.lag > 0).count();
        let (state_kind, state_label) = if member_views.is_empty() {
            ("idle", "no devices".to_string())
        } else if behind == 0 {
            ("ok", "up to date".to_string())
        } else {
            ("warn", format!("{behind} of {} behind", member_views.len()))
        };

        views.push(FolderView {
            id: folder.id,
            name: folder.name,
            state_kind: state_kind.to_string(),
            state_label,
            members: member_views,
            exclude: folder.exclude,
            preserve_permissions: folder.preserve_permissions,
        });
    }
    Ok(views)
}

/// Turn a member's acked watermark into a rail position and a label. With no
/// ops in the folder there is nothing to be behind of, so the rail reads full.
fn member_view(member: members::Member, head: i64) -> MemberView {
    let lag = (head - member.acked_seq).max(0);
    let pct = if head <= 0 || lag == 0 {
        100
    } else {
        (member.acked_seq.max(0) * 100 / head).clamp(0, 100)
    };
    let (lag_label, state_label) = if lag == 0 {
        ("in sync".to_string(), "in sync".to_string())
    } else {
        (format!("{lag} behind"), format!("{lag} ops behind"))
    };
    MemberView {
        device_name: member.device_name,
        root: display_path(&member.root_path),
        lag,
        pct,
        lag_label,
        state_label,
    }
}

/// Every device with its presence state and a human age of the last contact.
async fn device_views(state: &AppState) -> Result<Vec<DeviceView>, AppError> {
    let devices = state.store.blocking(devices::list_with_seen).await?;
    Ok(devices
        .into_iter()
        .map(|device| {
            let (state_kind, state_label) = if device.revoked {
                ("bad", "revoked".to_string())
            } else if device.seen_seconds.is_none() {
                ("warn", "never seen".to_string())
            } else {
                ("ok", "active".to_string())
            };
            DeviceView {
                name: device.name,
                id: device.id,
                state_kind: state_kind.to_string(),
                state_label,
                seen: device.seen_seconds.map(human_age).unwrap_or_default(),
            }
        })
        .collect())
}

/// The single sentence the overview leads with. Pure, so it is unit-tested.
///
/// Lag (`behind > 0`) means devices have not converged yet and takes precedence.
/// Overwrites are last-write-wins history (both versions are kept for rollback),
/// so once every device has caught up they do not raise a fault banner.
fn status_summary(
    overwrites: i64,
    behind: i64,
    online: i64,
    folders: i64,
    devices_total: i64,
) -> (String, String, String) {
    if behind > 0 {
        (
            "warn".to_string(),
            format!("{} behind", count(behind, "device")),
            "A device is behind on a folder's change log. It catches up on the next sync."
                .to_string(),
        )
    } else if devices_total == 0 {
        (
            "idle".to_string(),
            "Nothing is syncing yet".to_string(),
            "Add a device, then run `omni-sync add` to choose a folder.".to_string(),
        )
    } else if overwrites > 0 {
        (
            "ok".to_string(),
            "All clear".to_string(),
            format!(
                "{}, {} — {} kept for rollback",
                count(online, "device"),
                count(folders, "folder"),
                count(overwrites, "superseded version")
            ),
        )
    } else {
        (
            "ok".to_string(),
            "All clear".to_string(),
            format!(
                "{}, {}, nothing to review",
                count(online, "device"),
                count(folders, "folder")
            ),
        )
    }
}

fn count(n: i64, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// A compact age, e.g. `just now`, `4 min ago`, `yesterday`, `3 weeks ago`.
fn human_age(seconds: i64) -> String {
    let seconds = seconds.max(0);
    match seconds {
        s if s < 45 => "just now".to_string(),
        s if s < 90 => "1 min ago".to_string(),
        s if s < 3_600 => format!("{} min ago", s / 60),
        s if s < 5_400 => "1 hour ago".to_string(),
        s if s < 86_400 => format!("{} hours ago", s / 3_600),
        s if s < 172_800 => "yesterday".to_string(),
        s if s < 1_209_600 => format!("{} days ago", s / 86_400),
        s if s < 2_592_000 => format!("{} weeks ago", s / 604_800),
        s => format!("{} months ago", s / 2_592_000),
    }
}

/// Bytes at a human scale, e.g. `812 B`, `14.2 KiB`.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Render raw path bytes for display. Lossy on purpose: this is a web page, not
/// the sync path, and a replacement character is honest about undecodable names.
fn display_path(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

fn login_page(error: Option<&str>, next: &str, _headers: &HeaderMap) -> Result<String, AppError> {
    let page = Login {
        version: env!("CARGO_PKG_VERSION").to_string(),
        error: error.map(str::to_owned),
        next: next.to_owned(),
    };
    Ok(page.render()?)
}

/// Only allow same-origin redirect targets, so `?next=` cannot bounce a logged-in
/// operator to another host.
///
/// Browsers normalise a backslash to `/` in a URL, so `/\evil.example` is a
/// protocol-relative redirect to `evil.example` even though it does not start
/// with `//`. Control bytes would either split the header or be silently
/// stripped; both are refused rather than repaired.
fn safe_next(next: &str) -> &str {
    if !next.starts_with('/')
        || next.starts_with("//")
        || next.bytes().any(|b| b == b'\\' || b < 0x20 || b == 0x7f)
        || contains_encoded_redirect_control(next)
    {
        "/"
    } else {
        next
    }
}

/// Reject percent-encoded backslash/CR/LF in a redirect target. Browsers do
/// not treat these as a host separator, but refusing them keeps the rule easy
/// to reason about and blocks a future decoder from turning one into a control.
fn contains_encoded_redirect_control(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index + 2 < bytes.len() {
        if bytes[index] == b'%'
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            let byte = (high << 4) | low;
            if matches!(byte, b'\\' | b'\r' | b'\n') {
                return true;
            }
        }
        index += 1;
    }
    false
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Should the session cookie carry `Secure`?
///
/// Only when the deployment explicitly says it is behind a TLS-terminating
/// proxy and that proxy says the original request was HTTPS. Without
/// `trust_forwarded_proto`, a direct request could set the header itself and
/// either downgrade the cookie or claim HTTPS it does not have. Plain HTTP
/// (including localhost development) never gets `Secure`.
fn is_https(state: &AppState, headers: &HeaderMap) -> bool {
    if !state.config.trust_forwarded_proto {
        return false;
    }
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(|proto| proto.trim().eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

/// A rate-limit key for login/enrollment.
///
/// Direct callers are keyed by their socket address, never an untrusted header.
/// A trusted proxy must append the verified source to X-Forwarded-For: only the
/// last hop is used, so attacker-provided earlier entries cannot reset a budget.
fn client_key(state: &AppState, headers: &HeaderMap, peer: Option<std::net::SocketAddr>) -> String {
    if state.config.trust_forwarded_proto
        && let Some(ip) = headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.rsplit(',').next())
            .and_then(|value| value.trim().parse::<std::net::IpAddr>().ok())
    {
        return ip.to_string();
    }
    peer.map(|peer| peer.ip().to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn session_ok(state: &AppState, headers: &HeaderMap) -> bool {
    cookie(headers, SESSION_COOKIE)
        .map(|handle| state.session_valid(handle))
        .unwrap_or(false)
}

/// Read one cookie value from the `Cookie` header.
fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value)
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request};
    use serde::de::DeserializeOwned;
    use serde_json::{Value, json};
    use tower::ServiceExt as _;

    use omni_netproto::{HeadResponse, ManifestResponse, SubmitOpsResponse};
    use omni_store::{devices, folders};

    const TOKEN: &str = "osn_test_token";

    struct TestApp {
        router: Router,
        state: AppState,
        _dir: tempfile::TempDir,
    }

    fn test_app() -> TestApp {
        test_app_with(false, String::new())
    }

    fn test_app_with(trust_forwarded_proto: bool, dashboard_password_hash: String) -> TestApp {
        test_app_with_ttl(
            trust_forwarded_proto,
            dashboard_password_hash,
            std::time::Duration::from_secs(24 * 60 * 60),
        )
    }

    fn test_app_with_ttl(
        trust_forwarded_proto: bool,
        dashboard_password_hash: String,
        session_ttl: std::time::Duration,
    ) -> TestApp {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::Config {
            addr: "127.0.0.1:0".into(),
            data_dir: dir.path().join("data"),
            state_dir: dir.path().join("state"),
            dashboard_password_hash,
            trust_forwarded_proto,
        };
        let state = AppState::with_session_ttl(
            omni_store::Store::open_in_memory().unwrap(),
            config,
            session_ttl,
        );
        let router = router(state.clone());
        TestApp {
            router,
            state,
            _dir: dir,
        }
    }

    fn seed_device(app: &TestApp) {
        let device = devices::Device {
            id: "dev-1".into(),
            name: "test".into(),
            pubkey: Vec::new(),
            token_hash: auth::hash_token(TOKEN),
            revoked: false,
            last_seen_at: None,
        };
        app.state
            .store
            .with(|conn| devices::insert(conn, &device))
            .unwrap();
    }

    fn seed_folder(app: &TestApp, id: &str) {
        let folder = folders::Folder::new(id, "Documents");
        app.state
            .store
            .with(|conn| folders::create(conn, &folder))
            .unwrap();
    }

    async fn call(app: &TestApp, request: Request<Body>) -> Response {
        app.router.clone().oneshot(request).await.unwrap()
    }

    fn api_request(method: Method, uri: &str, body: Body) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(body)
            .unwrap()
    }

    fn json_request(method: Method, uri: &str, value: Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&value).unwrap()))
            .unwrap()
    }

    async fn json_body<T: DeserializeOwned>(response: Response) -> T {
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Upload a chunk through the real endpoint, so the `chunks` row and the
    /// on-disk file are both created the way a client would create them.
    async fn store_chunk(app: &TestApp, bytes: &[u8]) -> String {
        let hash = omni_chunk::hash_hex(bytes);
        let request = api_request(
            Method::PUT,
            &format!("/api/v1/chunks/{hash}"),
            Body::from(bytes.to_vec()),
        );
        let response = call(app, request).await;
        assert_eq!(response.status(), StatusCode::OK);
        hash
    }

    async fn put_file(app: &TestApp, folder: &str, path: &[u8], bytes: &[u8]) -> i64 {
        let hash = store_chunk(app, bytes).await;
        let payload = json!({
            "ops": [{
                "kind": "put",
                "path": omni_netproto::encode_path(path),
                "size": bytes.len(),
                "mtime_ns": 1,
                "mode": 0o644,
                "chunks": [{"hash": hash, "len": bytes.len()}],
            }]
        });
        let response = call(
            app,
            json_request(
                Method::POST,
                &format!("/api/v1/folders/{folder}/ops"),
                payload,
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let applied: SubmitOpsResponse = json_body(response).await;
        applied.applied[0].version_id
    }

    async fn manifest(app: &TestApp, folder: &str) -> ManifestResponse {
        let response = call(
            app,
            api_request(
                Method::GET,
                &format!("/api/v1/folders/{folder}/manifest"),
                Body::empty(),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        json_body(response).await
    }

    async fn head(app: &TestApp, folder: &str) -> i64 {
        let response = call(
            app,
            api_request(
                Method::GET,
                &format!("/api/v1/folders/{folder}/head"),
                Body::empty(),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let head: HeadResponse = json_body(response).await;
        head.head
    }

    #[test]
    fn next_only_accepts_same_origin_paths() {
        assert_eq!(safe_next("/folders"), "/folders");
        assert_eq!(safe_next("//evil.example"), "/");
        assert_eq!(safe_next("https://evil.example"), "/");
        // A browser treats `\` like `/`; `/\evil.example` is protocol-relative.
        assert_eq!(safe_next("/\\evil.example"), "/");
        assert_eq!(safe_next("/%5Cevil.example"), "/");
        assert_eq!(safe_next("/%0d%0aX-Injected: 1"), "/");
        assert_eq!(safe_next("/folders\r\nX-Injected: 1"), "/");
        assert_eq!(safe_next("/folders\n"), "/");
    }

    #[test]
    fn cookies_are_read_by_name() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; omni_session=xyz"),
        );
        assert_eq!(cookie(&headers, "omni_session"), Some("xyz"));
        assert_eq!(cookie(&headers, "absent"), None);
    }

    #[test]
    fn bearer_tokens_are_parsed_and_trimmed() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer osn_abc"),
        );
        assert_eq!(bearer(&headers), Some("osn_abc"));

        let mut empty = HeaderMap::new();
        empty.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic x"));
        assert_eq!(bearer(&empty), None);
    }

    #[test]
    fn display_path_is_lossy_but_never_panics() {
        assert_eq!(display_path(b"notes/todo.md"), "notes/todo.md");
        // A non-UTF-8 byte becomes the replacement character, not an error.
        assert!(display_path(&[0xff, b'a']).ends_with('a'));
    }

    #[test]
    fn only_loopback_callbacks_are_accepted() {
        assert!(is_loopback_callback("http://127.0.0.1:51234"));
        assert!(is_loopback_callback("http://localhost:8080"));
        assert!(is_loopback_callback("http://LOCALHOST:8080"));
        assert!(is_loopback_callback("http://[::1]:9"));
        // A redirect to any other host would be an open redirect.
        assert!(!is_loopback_callback("https://evil.example/"));
        assert!(!is_loopback_callback("https://127.0.0.1:8080"));
        assert!(!is_loopback_callback("http://10.0.0.5:80"));
        assert!(!is_loopback_callback("http://127.0.0.1:80\r\nX: y"));
        // Prefix-matching used to accept this: the browser goes to evil.example.
        assert!(!is_loopback_callback("http://127.0.0.1:1234@evil.example/"));
        assert!(!is_loopback_callback("http://user:pass@127.0.0.1:1"));
        assert!(!is_loopback_callback("http://127.0.0.1:1234#frag"));
        // A path or query stays on the loopback origin, so it is allowed.
        assert!(is_loopback_callback("http://127.0.0.1:1234?x=1"));
        assert!(is_loopback_callback("http://127.0.0.1:1234/callback"));
        assert!(!is_loopback_callback("http://127.0.0.1"));
        assert!(!is_loopback_callback("http://127.0.0.1:0"));
        assert!(!is_loopback_callback("http://127.0.0.1:99999"));
        assert!(!is_loopback_callback("http://127.0.0.1:abc"));
        assert!(!is_loopback_callback("http://127.0.0.1:12\\@evil.example/"));
        assert!(!is_loopback_callback("http://2130706433:1234/"));
        assert!(!is_loopback_callback("http://127.1:1234/"));
        assert!(!is_loopback_callback("http://localhost.evil.example:1234/"));
    }

    #[test]
    fn forwarding_headers_cannot_reset_the_authentication_budget() {
        let state = AppState::new(
            omni_store::Store::open_in_memory().unwrap(),
            crate::config::Config {
                trust_forwarded_proto: true,
                ..Default::default()
            },
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("spoofed-first, 192.0.2.1"),
        );
        let first = client_key(&state, &headers, None);
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("different-spoof, 192.0.2.1"),
        );
        assert_eq!(first, client_key(&state, &headers, None));
        let direct = AppState::new(
            omni_store::Store::open_in_memory().unwrap(),
            Default::default(),
        );
        assert_eq!(
            client_key(&direct, &headers, Some("192.0.2.9:10".parse().unwrap())),
            "192.0.2.9"
        );
        assert_ne!(
            client_key(&direct, &headers, Some("192.0.2.8:10".parse().unwrap())),
            "192.0.2.9"
        );
    }

    #[test]
    fn percent_encoding_neutralises_injection() {
        assert_eq!(percent_encode("ABCD-EFGHJ"), "ABCD-EFGHJ");
        assert_eq!(percent_encode("a b&c"), "a%20b%26c");
        assert_eq!(percent_encode("\r\n"), "%0D%0A");
    }

    #[test]
    fn ages_are_human_and_monotonic() {
        assert_eq!(human_age(3), "just now");
        assert_eq!(human_age(60), "1 min ago");
        assert_eq!(human_age(600), "10 min ago");
        assert_eq!(human_age(7_200), "2 hours ago");
        assert_eq!(human_age(100_000), "yesterday");
        assert_eq!(human_age(3 * 86_400), "3 days ago");
        assert_eq!(human_age(14 * 86_400), "2 weeks ago");
        // A clock skew that puts a device "in the future" is clamped, not negative.
        assert_eq!(human_age(-5), "just now");
    }

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(812), "812 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1024 * 1024 * 3 / 2), "1.5 MiB");
    }

    #[test]
    fn the_overview_leads_with_the_most_urgent_state() {
        // Lag (genuine divergence) outranks converged overwrites.
        let (kind, head, _) = status_summary(1, 3, 2, 1, 2);
        assert_eq!(kind, "warn");
        assert_eq!(head, "3 devices behind");
        // Converged overwrites are calm rollback history, not a fault.
        let (kind, head, sub) = status_summary(1, 0, 2, 1, 2);
        assert_eq!(kind, "ok");
        assert_eq!(head, "All clear");
        assert_eq!(
            sub,
            "2 devices, 1 folder — 1 superseded version kept for rollback"
        );
        // Nothing enrolled.
        let (kind, _, _) = status_summary(0, 0, 0, 0, 0);
        assert_eq!(kind, "idle");
        // Otherwise all clear, with counts that read naturally at 1 and n.
        let (kind, head, sub) = status_summary(0, 0, 2, 1, 2);
        assert_eq!(kind, "ok");
        assert_eq!(head, "All clear");
        assert_eq!(sub, "2 devices, 1 folder, nothing to review");
    }

    #[test]
    fn a_member_with_no_log_ahead_reads_in_sync() {
        let member = |acked_seq: i64| members::Member {
            device_id: "d1".into(),
            device_name: "laptop".into(),
            root_path: b"/docs".to_vec(),
            mode: "both".into(),
            acked_seq,
        };
        // Empty folder: nothing to be behind of.
        let view = member_view(member(0), 0);
        assert_eq!(view.pct, 100);
        assert_eq!(view.lag_label, "in sync");

        // Four of ten ops acked: the rail is 40% full and the gap is named.
        let view = member_view(member(4), 10);
        assert_eq!(view.pct, 40);
        assert_eq!(view.lag, 6);
        assert_eq!(view.lag_label, "6 behind");

        // Caught up.
        let view = member_view(member(10), 10);
        assert_eq!(view.pct, 100);
        assert_eq!(view.lag, 0);
    }

    #[tokio::test]
    async fn a_preflight_for_a_chunk_put_is_allowed() {
        let app = test_app();

        // What a browser sends before a cross-origin `PUT /api/v1/chunks/<hash>`.
        let request = Request::builder()
            .method(Method::OPTIONS)
            .uri("/api/v1/chunks/abc")
            .header(header::ORIGIN, "http://localhost:8080")
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "PUT")
            .header(
                header::ACCESS_CONTROL_REQUEST_HEADERS,
                "authorization,content-type",
            )
            .body(Body::empty())
            .unwrap();

        let response = app.router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "*"
        );
        let methods = response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_METHODS)
            .expect("a preflight must allow methods")
            .to_str()
            .unwrap();
        // tower-http mirrors the requested method for `Any`; `*` is its other
        // valid rendering. Either way PUT must be permitted.
        assert!(methods == "*" || methods.contains("PUT"), "got {methods:?}");
        // The API is token-authenticated; ambient credentials must stay off.
        assert!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_batch_with_a_missing_chunk_is_a_conflict_and_commits_nothing() {
        let app = test_app();
        seed_device(&app);
        seed_folder(&app, "f1");
        let stored = store_chunk(&app, b"hello").await;
        let missing = omni_chunk::hash_hex(b"never uploaded");

        let payload = json!({
            "ops": [
                {
                    "kind": "put",
                    "path": omni_netproto::encode_path(b"good.txt"),
                    "size": 5,
                    "mtime_ns": 1,
                    "mode": 0o644,
                    "chunks": [{"hash": stored, "len": 5}],
                },
                {
                    "kind": "put",
                    "path": omni_netproto::encode_path(b"bad.txt"),
                    "size": 14,
                    "mtime_ns": 1,
                    "mode": 0o644,
                    "chunks": [{"hash": missing, "len": 14}],
                },
            ]
        });
        let response = call(
            &app,
            json_request(Method::POST, "/api/v1/folders/f1/ops", payload),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);

        // Neither op may have landed, including the one whose chunk exists.
        assert_eq!(head(&app, "f1").await, 0);
        assert!(manifest(&app, "f1").await.entries.is_empty());
    }

    #[tokio::test]
    async fn path_attacks_are_rejected_and_nothing_is_committed() {
        let app = test_app();
        seed_device(&app);
        seed_folder(&app, "f1");

        for path in [
            &b"../escape"[..],
            b"/etc/passwd",
            b"notes/../../escape",
            b"a\0b",
            b"notes\\todo.md",
            b"C:evil",
            b"\\\\server\\share",
            b"a/../b",
            b"a//b",
            b"",
        ] {
            let payload = json!({
                "ops": [{
                    "kind": "put",
                    "path": omni_netproto::encode_path(path),
                    "size": 0,
                    "mtime_ns": 0,
                    "mode": 0,
                    "chunks": [],
                }]
            });
            let response = call(
                &app,
                json_request(Method::POST, "/api/v1/folders/f1/ops", payload),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "should reject {:?}",
                String::from_utf8_lossy(path)
            );
        }
        assert_eq!(head(&app, "f1").await, 0);
        assert!(manifest(&app, "f1").await.entries.is_empty());
    }

    #[tokio::test]
    async fn resolve_refuses_a_version_from_another_folder() {
        let app = test_app();
        seed_device(&app);
        seed_folder(&app, "f1");
        seed_folder(&app, "f2");
        let version = put_file(&app, "f1", b"a.txt", b"hello").await;

        let payload = json!({
            "path": omni_netproto::encode_path(b"a.txt"),
            "restored_from": version,
        });
        let response = call(
            &app,
            json_request(Method::POST, "/api/v1/folders/f2/resolve", payload),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(head(&app, "f2").await, 0);
    }

    #[tokio::test]
    async fn resolve_uses_the_requested_path_as_its_active_base() {
        let app = test_app();
        seed_device(&app);
        seed_folder(&app, "f1");
        let source_version = put_file(&app, "f1", b"a.txt", b"one").await;
        put_file(&app, "f1", b"b.txt", b"two").await;

        // Restore a's content at b. b has an active version, so a correct
        // lookup finds it and this is a linear resolve, not an overwrite.
        let payload = json!({
            "path": omni_netproto::encode_path(b"b.txt"),
            "restored_from": source_version,
        });
        let response = call(
            &app,
            json_request(Method::POST, "/api/v1/folders/f1/resolve", payload),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let response = call(
            &app,
            api_request(Method::GET, "/api/v1/overwrites", Body::empty()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let overwrites: Vec<Value> = json_body(response).await;
        assert!(
            overwrites.is_empty(),
            "a resolve at an existing path must not look like an overwrite: {overwrites:?}"
        );
    }

    #[tokio::test]
    async fn resolve_refuses_a_version_whose_chunks_are_missing_on_disk() {
        let app = test_app();
        seed_device(&app);
        seed_folder(&app, "f1");
        let bytes = b"hello";
        let version = put_file(&app, "f1", b"a.txt", bytes).await;

        let hash = omni_chunk::hash_hex(bytes);
        let path = app.state.config.chunks_dir().join(&hash[..2]).join(&hash);
        std::fs::remove_file(path).unwrap();

        let payload = json!({
            "path": omni_netproto::encode_path(b"a.txt"),
            "restored_from": version,
        });
        let response = call(
            &app,
            json_request(Method::POST, "/api/v1/folders/f1/resolve", payload),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn a_loopback_callback_redirects_but_userinfo_does_not() {
        let app = test_app();

        let valid = Request::builder()
            .uri("/enroll?code=ABCD&cb=http://localhost:49152")
            .body(Body::empty())
            .unwrap();
        let response = call(&app, valid).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "http://localhost:49152?code=ABCD"
        );

        // Everything before `@` looks like a loopback prefix, but the browser
        // would go to evil.example. It must render the page, not redirect.
        let attack = Request::builder()
            .uri("/enroll?code=ABCD&cb=http://127.0.0.1:1234@evil.example/")
            .body(Body::empty())
            .unwrap();
        let response = call(&app, attack).await;
        assert_ne!(response.status(), StatusCode::SEE_OTHER);
        assert!(response.headers().get(header::LOCATION).is_none());
    }

    fn login_request(forwarded_proto: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(proto) = forwarded_proto {
            builder = builder.header("x-forwarded-proto", proto);
        }
        builder
            .body(Body::from("password=secret&next=%2Ffolders"))
            .unwrap()
    }

    #[tokio::test]
    async fn the_session_cookie_is_secure_only_when_https_is_trusted() {
        let phc = auth::hash_password("secret").unwrap();

        // Plain HTTP, even with a forged header, must not claim Secure.
        let app = test_app_with(false, phc.clone());
        let response = call(&app, login_request(Some("https"))).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(!cookie.contains("Secure"), "untrusted header: {cookie}");

        // Trusted proxy + https is the one combination that sets Secure.
        let app = test_app_with(true, phc.clone());
        let response = call(&app, login_request(Some("https"))).await;
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(cookie.contains("; Secure"), "trusted https: {cookie}");

        // Trusted proxy but plain HTTP (localhost development) stays usable.
        let app = test_app_with(true, phc);
        let response = call(&app, login_request(None)).await;
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(!cookie.contains("Secure"), "plain http: {cookie}");
    }

    #[tokio::test]
    async fn an_expired_session_cookie_is_sent_back_to_login() {
        let app = test_app_with_ttl(
            false,
            auth::hash_password("secret").unwrap(),
            std::time::Duration::from_millis(10),
        );
        let response = call(&app, login_request(None)).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        let handle = cookie.split(';').next().unwrap().to_string();

        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        let request = Request::builder()
            .uri("/")
            .header(header::COOKIE, handle)
            .body(Body::empty())
            .unwrap();
        let response = call(&app, request).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers().get(header::LOCATION).unwrap(), "/login");
    }

    #[tokio::test]
    async fn enrollment_attempts_are_rate_limited() {
        let app = test_app();
        let payload = json!({
            "code": "NOPE",
            "name": "test",
            "protocol_version": PROTOCOL_VERSION,
        });
        for _ in 0..10 {
            let response = call(
                &app,
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/devices/enroll")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&payload).unwrap()))
                    .unwrap(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        let response = call(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/devices/enroll")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&payload).unwrap()))
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn one_source_exhausting_its_budget_does_not_lock_out_other_sources() {
        let app = test_app();
        let payload = serde_json::to_vec(&json!({
            "code": "INVALID", "name": "test", "protocol_version": PROTOCOL_VERSION,
        }))
        .unwrap();
        let request = |source: &str| {
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/devices/enroll")
                .extension(axum::extract::ConnectInfo(
                    source.parse::<std::net::SocketAddr>().unwrap(),
                ))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(payload.clone()))
                .unwrap()
        };
        for _ in 0..10 {
            assert_eq!(
                call(&app, request("192.0.2.1:1234")).await.status(),
                StatusCode::BAD_REQUEST
            );
        }
        assert_eq!(
            call(&app, request("192.0.2.1:9999")).await.status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            call(&app, request("192.0.2.2:1234")).await.status(),
            StatusCode::BAD_REQUEST
        );
    }
}
