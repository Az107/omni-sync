//! The axum application: `/api/v1` plus the dashboard.

use askama::Template as _;
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use omni_netproto::{EnrollRequest, EnrollResponse, HealthResponse, PROTOCOL_VERSION};
use omni_store::{StoreError, devices, folders, pairing};

use crate::auth;
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
        .route("/api/v1/devices/enroll", post(enroll))
        .with_state(state)
}

/// Everything a handler can fail with.
pub enum AppError {
    Store(StoreError),
    Template(askama::Error),
    BadRequest(String),
    Internal(String),
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

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
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
        };
        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}

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
}
