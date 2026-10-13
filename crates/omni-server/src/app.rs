//! The axum application: `/api/v1` for devices, plus the dashboard.

use askama::Template as _;
use axum::body::Bytes;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;

use omni_core::{Op, OpKind};
use omni_netproto::{
    AppliedOpDto, ChunkPutResponse, CreateFolderRequest, EnrollRequest, EnrollResponse, FolderDto,
    FoldersResponse, HealthResponse, LogEntryDto, ManifestEntryDto, ManifestResponse, MemberDto,
    MembersResponse, OpKindDto, OpsResponse, OverwriteDto, PROTOCOL_VERSION, RegisterMemberRequest,
    ResolveRequest, SubmitOpsRequest, SubmitOpsResponse,
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
        .route("/overwrites", get(overwrites_page))
        .route("/overwrites/{id}/resolve", post(overwrite_resolve))
        // Device enrollment.
        .route("/api/v1/devices/enroll", post(enroll))
        // Folders.
        .route("/api/v1/folders", post(create_folder).get(list_folders))
        .route("/api/v1/folders/{id}/manifest", get(manifest))
        .route(
            "/api/v1/folders/{id}/members",
            post(register_member).get(list_members),
        )
        .route("/api/v1/folders/{id}/ops", post(submit_ops).get(list_ops))
        .route("/api/v1/folders/{id}/resolve", post(resolve))
        .route("/api/v1/overwrites", get(list_overwrites))
        // Chunks.
        .route("/api/v1/chunks/{hash}", put(put_chunk).get(get_chunk))
        .with_state(state)
}

/// Everything a handler can fail with.
pub enum AppError {
    Store(StoreError),
    Template(askama::Error),
    BadRequest(String),
    Unauthorized,
    NotFound,
    Internal(String),
    Chunk(ChunkError),
}

impl From<StoreError> for AppError {
    fn from(error: StoreError) -> Self {
        AppError::Store(error)
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
            ChunkError::Mismatch { .. } | ChunkError::InvalidHash(_) => {
                AppError::BadRequest(error.to_string())
            }
            ChunkError::Io(_) => AppError::Internal(error.to_string()),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized".to_string()),
            AppError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
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
        let folder = folders::Folder {
            id: uuid::Uuid::now_v7().to_string(),
            name: name.to_string(),
            retention_versions: 50,
            retention_days: 90,
            encrypted: false,
        };
        state
            .store
            .blocking(move |conn| folders::create(conn, &folder))
            .await?;
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
    Form(form): Form<LoginForm>,
) -> Result<Response, AppError> {
    if state.config.dashboard_password_hash.is_empty() {
        return Ok(Html(login_page(
            Some("no dashboard password is set — run `omni-sync-server init`"),
            "/",
            &HeaderMap::new(),
        )?)
        .into_response());
    }
    if !auth::verify_password(&form.password, &state.config.dashboard_password_hash) {
        return Ok(Html(login_page(
            Some("incorrect password"),
            safe_next(&form.next),
            &HeaderMap::new(),
        )?)
        .into_response());
    }

    let handle = state.create_session();
    let target = safe_next(&form.next);
    let mut response = Redirect::to(target).into_response();
    let cookie =
        format!("{SESSION_COOKIE}={handle}; HttpOnly; SameSite=Strict; Path=/; Max-Age=86400");
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
    let cookie = format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0");
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
fn is_loopback_callback(cb: &str) -> bool {
    if cb.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return false;
    }
    ["http://127.0.0.1:", "http://localhost:", "http://[::1]:"]
        .iter()
        .any(|prefix| cb.starts_with(prefix))
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
    Json(request): Json<EnrollRequest>,
) -> Result<Json<EnrollResponse>, AppError> {
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
    Ok(Json(EnrollResponse { device_id, token }))
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
    let folder = folders::Folder {
        id: uuid::Uuid::now_v7().to_string(),
        name: name.to_string(),
        retention_versions: 50,
        retention_days: 90,
        encrypted: false,
    };
    let created = folder.clone();
    state
        .store
        .blocking(move |conn| folders::create(conn, &created))
        .await?;
    Ok(Json(folder_dto(&folder)))
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
            members::upsert(conn, &folder, &device_id, &root_path, &mode)?;
            members::list(conn, &folder)
        })
        .await?
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

    let mut applied = Vec::new();
    let mut overwrites = Vec::new();
    for dto in &request.ops {
        let op = dto
            .to_core()
            .map_err(|_| AppError::BadRequest("a path is not valid base64".into()))?;
        let result = state.apply(&folder_id, &device.id, op).await?;
        applied.push(AppliedOpDto {
            seq: result.seq,
            version_id: result.version_id,
        });
        if let Some(overwrite) = result.overwrite {
            overwrites.push(OverwriteDto {
                path: dto.path.clone(),
                losing_version_id: overwrite.losing_version_id,
                winning_version_id: overwrite.winning_version_id,
            });
        }
    }

    Ok(Json(SubmitOpsResponse {
        applied,
        overwrites,
    }))
}

#[derive(Deserialize)]
struct SinceQuery {
    #[serde(default)]
    since: i64,
}

async fn list_ops(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(folder_id): Path<String>,
    Query(query): Query<SinceQuery>,
) -> Result<Json<OpsResponse>, AppError> {
    let device = authenticate(&state, &headers).await?;
    let since = query.since;
    let lookup = folder_id.clone();
    let entries = state
        .store
        .blocking(move |conn| sync::log_since(conn, &lookup, since))
        .await?;

    // A pull also tells us how far this device has caught up: it will apply
    // everything returned, so its watermark becomes the last seq (or its
    // `since` when nothing is new). This is what the dashboard's rail shows.
    let acked = entries.last().map(|entry| entry.seq).unwrap_or(since);
    let folder = folder_id.clone();
    let device_id = device.id.clone();
    state
        .store
        .blocking(move |conn| members::set_acked(conn, &folder, &device_id, acked))
        .await?;

    Ok(Json(OpsResponse {
        entries: entries.into_iter().map(log_dto).collect(),
    }))
}

async fn list_overwrites(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<OverwriteDto>>, AppError> {
    let _device = authenticate(&state, &headers).await?;
    let rows = state
        .store
        .blocking(|conn| sync::overwrites(conn, true))
        .await?;
    Ok(Json(rows.into_iter().map(overwrite_dto).collect()))
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

    let version_id = request.restored_from;
    let version = state
        .store
        .blocking(move |conn| sync::version(conn, version_id))
        .await?
        .ok_or_else(|| AppError::BadRequest(format!("unknown version {version_id}")))?;

    let folder = folder_id.clone();
    let lookup_path = version.path.clone();
    let active = state
        .store
        .blocking(move |conn| sync::active_version_id(conn, &folder, &lookup_path))
        .await?;

    // Restore the content under the requested path.
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
    let created = state.chunks.put(&hash, &body)?;
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
    match state.chunks.get(&hash)? {
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
        path: omni_netproto::encode_path(&row.path),
        losing_version_id: row.losing_version_id,
        winning_version_id: row.winning_version_id,
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
fn status_summary(
    overwrites: i64,
    behind: i64,
    online: i64,
    folders: i64,
    devices_total: i64,
) -> (String, String, String) {
    if overwrites > 0 {
        (
            "bad".to_string(),
            format!("{} to review", count(overwrites, "overwrite")),
            "Two devices changed the same file while apart. Keep one; the other stays stored."
                .to_string(),
        )
    } else if behind > 0 {
        (
            "warn".to_string(),
            format!("{} behind", count(behind, "device")),
            "A device is behind on a folder's change log. It catches up on the next poll."
                .to_string(),
        )
    } else if devices_total == 0 {
        (
            "idle".to_string(),
            "Nothing is syncing yet".to_string(),
            "Add a device, then run `omni-sync add` to choose a folder.".to_string(),
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
fn safe_next(next: &str) -> &str {
    if next.starts_with('/') && !next.starts_with("//") {
        next
    } else {
        "/"
    }
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

    #[test]
    fn next_only_accepts_same_origin_paths() {
        assert_eq!(safe_next("/folders"), "/folders");
        assert_eq!(safe_next("//evil.example"), "/");
        assert_eq!(safe_next("https://evil.example"), "/");
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
        assert!(is_loopback_callback("http://[::1]:9"));
        // A redirect to any other host would be an open redirect.
        assert!(!is_loopback_callback("https://evil.example/"));
        assert!(!is_loopback_callback("http://10.0.0.5:80"));
        assert!(!is_loopback_callback("http://127.0.0.1:80\r\nX: y"));
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
        // Overwrites outrank lag.
        let (kind, head, _) = status_summary(1, 3, 2, 1, 2);
        assert_eq!(kind, "bad");
        assert_eq!(head, "1 overwrite to review");
        // Lag next.
        let (kind, head, _) = status_summary(0, 2, 2, 1, 2);
        assert_eq!(kind, "warn");
        assert_eq!(head, "2 devices behind");
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
}
