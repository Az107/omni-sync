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
    FoldersResponse, HealthResponse, LogEntryDto, ManifestEntryDto, ManifestResponse, OpKindDto,
    OpsResponse, OverwriteDto, PROTOCOL_VERSION, ResolveRequest, SubmitOpsRequest,
    SubmitOpsResponse,
};
use omni_store::sync::{self, ActiveFile, LogEntry, OverwriteRow};
use omni_store::{StoreError, devices, folders, pairing};

use crate::auth;
use crate::chunkstore::ChunkError;
use crate::state::AppState;
use crate::templates::{Dashboard, Login};

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
        .route("/static/{*path}", get(static_asset))
        // Device enrollment.
        .route("/api/v1/devices/enroll", post(enroll))
        // Folders.
        .route("/api/v1/folders", post(create_folder).get(list_folders))
        .route("/api/v1/folders/{id}/manifest", get(manifest))
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
                _ => "application/octet-stream",
            };
            (
                [(header::CONTENT_TYPE, content_type)],
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
    let devices = state.store.blocking(devices::count).await?;
    let folders = state.store.blocking(folders::count).await?;
    let page = Dashboard {
        version: env!("CARGO_PKG_VERSION").to_string(),
        devices,
        folders,
    };
    Ok(Html(page.render()?).into_response())
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
    let _device = authenticate(&state, &headers).await?;
    let since = query.since;
    let entries = state
        .store
        .blocking(move |conn| sync::log_since(conn, &folder_id, since))
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
}
