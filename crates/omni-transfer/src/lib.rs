//! HTTP client for the server's `/api/v1`.
//!
//! The daemon's entire interaction with the network goes through this one type,
//! so the sync engine can be reasoned about without reading HTTP code, and the
//! client can be pointed at a test server.

use std::time::Duration;

use omni_netproto::{
    ChunkPutResponse, CreateFolderRequest, EnrollRequest, EnrollResponse, FolderDto,
    FoldersResponse, HistoryEntryDto, HistoryResponse, ManifestResponse, MembersResponse, OpDto,
    OpsResponse, PROTOCOL_VERSION, RegisterMemberRequest, ResolveRequest, SubmitOpsRequest,
    SubmitOpsResponse,
};
use reqwest::{RequestBuilder, Response, StatusCode, Url};
use serde::de::DeserializeOwned;

/// Connect deadline for every request. Without one, a black-holed route can
/// pin a daemon cycle (and therefore the control reply) for minutes.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Whole-request deadline for ordinary finite requests, including the body.
/// Chunks are at most ~1 MiB by construction, so this is generous even on a
/// slow link while still bounding a stuck server.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// The SSE stream is intentionally long-lived, so it must *not* inherit the
/// whole-request deadline. A read timeout does apply: the server sends an
/// axum keep-alive comment every 15 s, so a minute without any byte means the
/// connection is dead and the waker should reconnect.
const STREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Error bodies are diagnostic text, never a payload; cap what a hostile or
/// broken server can make us buffer.
const MAX_ERROR_BODY: usize = 64 * 1024;
/// Includes legacy unpaged manifests/tails while putting a hard ceiling on a
/// hostile finite JSON response. New op-log requests are paged well below it.
const MAX_JSON_BODY: usize = 64 * 1024 * 1024;
/// A configured chunker max is 1 MiB; allow generous headroom but refuse an
/// endless chunk download.
const MAX_CHUNK_BYTES: usize = omni_netproto::MAX_CHUNK_BYTES;
/// One SSE frame is a tiny event. A peer that never sends the blank-line
/// terminator must not grow the buffer forever.
const MAX_SSE_FRAME_BYTES: usize = 64 * 1024;

/// The server clamps a `limit` to 2000; ask for a comfortable page and keep
/// following `next` until the tail is drained.
const OPS_PAGE_LIMIT: usize = 500;
/// A single pull aggregates at most this many pages. If the tail is larger the
/// returned `next` lets the engine's next pull continue from where this one
/// stopped, so bounded memory does not skip ops.
const MAX_OPS_PAGES_PER_PULL: usize = 64;

/// Everything that can go wrong talking to the server.
#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("invalid server JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid server url: {0}")]
    Url(String),
    #[error("server refused the request ({status}): {body}")]
    Status { status: u16, body: String },
    #[error("{what} exceeds {limit} bytes")]
    TooLarge { what: &'static str, limit: usize },
}

/// A client for one server. The bearer token is attached to every call once set.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    stream: reqwest::Client,
    base: Url,
    token: Option<String>,
}

impl Client {
    pub fn new(base_url: &str) -> Result<Self, TransferError> {
        let mut base = Url::parse(base_url).map_err(|e| TransferError::Url(e.to_string()))?;
        // Normalise to a trailing slash so relative endpoints append *under* the
        // base path. That is what makes a sub-path deployment work, e.g. behind
        // space-elevator at https://host/app/omni-sync/.
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        // No overall `timeout`: the op stream is expected to stay open. The
        // read timeout catches a half-open connection between keep-alives.
        let stream = reqwest::Client::builder()
            .connect_timeout(STREAM_CONNECT_TIMEOUT)
            .read_timeout(STREAM_READ_TIMEOUT)
            .build()?;
        Ok(Self {
            http,
            stream,
            base,
            token: None,
        })
    }

    /// Attach the device token issued by enrollment.
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    fn endpoint(&self, path: &str) -> Result<Url, TransferError> {
        // Relative join: a leading '/' would reset to the host root and drop a
        // deployment's base path.
        self.base
            .join(path.trim_start_matches('/'))
            .map_err(|e| TransferError::Url(e.to_string()))
    }

    fn auth(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    /// Exchange a one-time pairing code for a device id and token.
    pub async fn enroll(&self, name: &str, code: &str) -> Result<EnrollResponse, TransferError> {
        let url = self.endpoint("/api/v1/devices/enroll")?;
        let request = EnrollRequest {
            code: code.to_owned(),
            name: name.to_owned(),
            protocol_version: PROTOCOL_VERSION,
        };
        let response = self.http.post(url).json(&request).send().await?;
        json(response).await
    }

    pub async fn create_folder(&self, name: &str) -> Result<FolderDto, TransferError> {
        self.create_folder_with(name, Vec::new(), true).await
    }

    /// Create a folder with an explicit sync policy (the vault preset, for
    /// example). The server stores it and every client follows it.
    pub async fn create_folder_with(
        &self,
        name: &str,
        exclude: Vec<String>,
        preserve_permissions: bool,
    ) -> Result<FolderDto, TransferError> {
        let url = self.endpoint("/api/v1/folders")?;
        let request = self.auth(self.http.post(url)).json(&CreateFolderRequest {
            name: name.to_owned(),
            exclude,
            preserve_permissions,
        });
        json(request.send().await?).await
    }

    /// Change a folder's sync policy. `None` leaves a field unchanged.
    pub async fn update_folder(
        &self,
        folder_id: &str,
        exclude: Option<Vec<String>>,
        preserve_permissions: Option<bool>,
    ) -> Result<FolderDto, TransferError> {
        let url = self.endpoint(&format!("/api/v1/folders/{folder_id}"))?;
        let request = self
            .auth(self.http.patch(url))
            .json(&omni_netproto::UpdateFolderRequest {
                exclude,
                preserve_permissions,
            });
        json(request.send().await?).await
    }

    pub async fn list_folders(&self) -> Result<Vec<FolderDto>, TransferError> {
        let url = self.endpoint("/api/v1/folders")?;
        let response: FoldersResponse = json(self.auth(self.http.get(url)).send().await?).await?;
        Ok(response.folders)
    }

    /// One folder, including its sync policy.
    pub async fn get_folder(&self, folder_id: &str) -> Result<FolderDto, TransferError> {
        let url = self.endpoint(&format!("/api/v1/folders/{folder_id}"))?;
        json(self.auth(self.http.get(url)).send().await?).await
    }

    /// Tell the server this device holds a folder at a root. `root_path` is raw
    /// bytes; it is base64-encoded for the wire.
    pub async fn register_member(
        &self,
        folder_id: &str,
        root_path: &[u8],
        mode: &str,
    ) -> Result<(), TransferError> {
        let url = self.endpoint(&format!("/api/v1/folders/{folder_id}/members"))?;
        let request = self.auth(self.http.post(url)).json(&RegisterMemberRequest {
            root_path: omni_netproto::encode_path(root_path),
            mode: mode.to_owned(),
        });
        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            let body = text_bounded(response, MAX_ERROR_BODY).await;
            return Err(TransferError::Status {
                status: status.as_u16(),
                body,
            });
        }
        Ok(())
    }

    pub async fn list_members(
        &self,
        folder_id: &str,
    ) -> Result<Vec<omni_netproto::MemberDto>, TransferError> {
        let url = self.endpoint(&format!("/api/v1/folders/{folder_id}/members"))?;
        let response: MembersResponse = json(self.auth(self.http.get(url)).send().await?).await?;
        Ok(response.members)
    }

    pub async fn manifest(&self, folder_id: &str) -> Result<ManifestResponse, TransferError> {
        let url = self.endpoint(&format!("/api/v1/folders/{folder_id}/manifest"))?;
        json(self.auth(self.http.get(url)).send().await?).await
    }

    pub async fn submit_ops(
        &self,
        folder_id: &str,
        ops: Vec<OpDto>,
    ) -> Result<SubmitOpsResponse, TransferError> {
        let url = self.endpoint(&format!("/api/v1/folders/{folder_id}/ops"))?;
        let request = self
            .auth(self.http.post(url))
            .json(&SubmitOpsRequest { ops });
        json(request.send().await?).await
    }

    /// Pull the op log from `since`, following the server's pages internally.
    ///
    /// The wire DTO already carries `next`/`has_more`; using them here keeps
    /// one response from loading an arbitrarily large tail into memory. An
    /// older server ignores `limit` and returns the whole tail with
    /// `has_more = false`, which this loop handles unchanged. If a single pull
    /// hits the page cap, the returned `next` is the cursor for the next pull,
    /// so no op is skipped.
    pub async fn ops_since(
        &self,
        folder_id: &str,
        since: i64,
    ) -> Result<OpsResponse, TransferError> {
        let mut entries = Vec::new();
        let mut cursor = since;
        let mut has_more = false;

        for _ in 0..MAX_OPS_PAGES_PER_PULL {
            let page = self
                .ops_page(folder_id, cursor, Some(OPS_PAGE_LIMIT))
                .await?;
            let last_seq = page.entries.last().map(|entry| entry.seq);
            entries.extend(page.entries);
            let (next, more) = advance_cursor(cursor, page.next, last_seq, page.has_more);
            cursor = next;
            has_more = more;
            if !more {
                break;
            }
        }

        Ok(OpsResponse {
            entries,
            next: cursor,
            has_more,
        })
    }

    /// One page of the op log. `limit = None` asks for the whole tail (the
    /// pre-pagination behaviour) and exists for callers that need it.
    pub async fn ops_page(
        &self,
        folder_id: &str,
        since: i64,
        limit: Option<usize>,
    ) -> Result<OpsResponse, TransferError> {
        let mut url = self.endpoint(&format!("/api/v1/folders/{folder_id}/ops?since={since}"))?;
        if let Some(limit) = limit {
            url.query_pairs_mut()
                .append_pair("limit", &limit.to_string());
        }
        json(self.auth(self.http.get(url)).send().await?).await
    }

    pub async fn history(
        &self,
        folder_id: &str,
        path: &[u8],
    ) -> Result<Vec<HistoryEntryDto>, TransferError> {
        let mut url = self.endpoint(&format!("/api/v1/folders/{folder_id}/history"))?;
        url.query_pairs_mut()
            .append_pair("path", &omni_netproto::encode_path(path));
        let response: HistoryResponse = json(self.auth(self.http.get(url)).send().await?).await?;
        Ok(response.entries)
    }

    pub async fn resolve(
        &self,
        folder_id: &str,
        path: &[u8],
        restored_from: i64,
    ) -> Result<SubmitOpsResponse, TransferError> {
        let url = self.endpoint(&format!("/api/v1/folders/{folder_id}/resolve"))?;
        let request = self.auth(self.http.post(url)).json(&ResolveRequest {
            path: omni_netproto::encode_path(path),
            restored_from,
        });
        json(request.send().await?).await
    }

    /// Upload a chunk. `false` means the server already had it.
    pub async fn put_chunk(&self, hash: &str, bytes: Vec<u8>) -> Result<bool, TransferError> {
        if bytes.len() > MAX_CHUNK_BYTES {
            return Err(TransferError::TooLarge {
                what: "chunk upload",
                limit: MAX_CHUNK_BYTES,
            });
        }
        let url = self.endpoint(&format!("/api/v1/chunks/{hash}"))?;
        let request = self
            .auth(self.http.put(url))
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes);
        let response: ChunkPutResponse = json(request.send().await?).await?;
        Ok(response.created)
    }

    /// Download a chunk. `None` when the server does not have it.
    pub async fn get_chunk(&self, hash: &str) -> Result<Option<Vec<u8>>, TransferError> {
        let url = self.endpoint(&format!("/api/v1/chunks/{hash}"))?;
        let response = self.auth(self.http.get(url)).send().await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let status = response.status();
        if !status.is_success() {
            let body = text_bounded(response, MAX_ERROR_BODY).await;
            return Err(TransferError::Status {
                status: status.as_u16(),
                body,
            });
        }
        let bytes = bytes_bounded(response, MAX_CHUNK_BYTES, "chunk download").await?;
        Ok(Some(bytes))
    }

    /// Open the server-sent event stream for a folder's op log.
    ///
    /// An event only says *that* the log grew; the ops themselves still come
    /// from [`Client::ops_since`], so the diff logic lives in one place.
    pub async fn stream_ops(&self, folder_id: &str) -> Result<OpStream, TransferError> {
        let url = self.endpoint(&format!("/api/v1/folders/{folder_id}/ops/stream"))?;
        // The streaming client has no whole-request deadline (the body is
        // long-lived) but does have a connect deadline and a read timeout.
        let response = self.auth(self.stream.get(url)).send().await?;
        let status = response.status();
        if !status.is_success() {
            let body = text_bounded(response, MAX_ERROR_BODY).await;
            return Err(TransferError::Status {
                status: status.as_u16(),
                body,
            });
        }
        Ok(OpStream {
            response,
            buffer: Vec::new(),
        })
    }
}

/// A live op-log stream for one folder.
///
/// The body is consumed incrementally: [`OpStream::next_head`] blocks until the
/// next event arrives and returns the head seq it carried.
pub struct OpStream {
    response: reqwest::Response,
    buffer: Vec<u8>,
}

impl OpStream {
    /// The head seq from the next event, or `None` when the stream ends.
    ///
    /// Keep-alive comments carry no head and are skipped.
    pub async fn next_head(&mut self) -> Result<Option<i64>, TransferError> {
        loop {
            if frame_too_large(&self.buffer) {
                return Err(TransferError::TooLarge {
                    what: "server-sent event frame",
                    limit: MAX_SSE_FRAME_BYTES,
                });
            }
            match take_frame(&mut self.buffer) {
                Frame::Ready(Some(head)) => return Ok(Some(head)),
                Frame::Ready(None) => continue,
                Frame::Incomplete => match self.response.chunk().await? {
                    Some(chunk) => self.buffer.extend_from_slice(&chunk),
                    None => return Ok(None),
                },
            }
        }
    }
}

/// One server-sent event frame, once a whole one is buffered.
enum Frame {
    /// A complete frame; the parsed `data:` payload, if it held one.
    Ready(Option<i64>),
    /// No frame separator yet — read more bytes.
    Incomplete,
}

/// Take one whole frame off the front of `buffer`.
///
/// A frame ends at a blank line. Comment lines (keep-alives) start with `:` and
/// carry no `data:`; head events do.
fn take_frame(buffer: &mut Vec<u8>) -> Frame {
    let Some((end, next)) = blank_line(buffer) else {
        return Frame::Incomplete;
    };
    let frame: Vec<u8> = buffer.drain(..next).take(end).collect();
    let mut head = None;
    for line in frame.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(value) = line.strip_prefix(b"data:") {
            let value = value.strip_prefix(b" ").unwrap_or(value);
            head = std::str::from_utf8(value)
                .ok()
                .and_then(|value| value.trim().parse().ok());
        }
    }
    Frame::Ready(head)
}

/// The blank line ending a frame, as `(content_end, next_frame_start)`.
fn blank_line(buffer: &[u8]) -> Option<(usize, usize)> {
    let mut index = 0;
    while index + 1 < buffer.len() {
        if &buffer[index..index + 2] == b"\n\n" {
            return Some((index, index + 2));
        }
        if index + 3 < buffer.len() && &buffer[index..index + 4] == b"\r\n\r\n" {
            return Some((index, index + 4));
        }
        index += 1;
    }
    None
}

/// Whether the buffered SSE bytes already exceed one frame's allowance.
fn frame_too_large(buffer: &[u8]) -> bool {
    match blank_line(buffer) {
        Some((end, _)) => end > MAX_SSE_FRAME_BYTES,
        None => buffer.len() > MAX_SSE_FRAME_BYTES,
    }
}

/// Compute the next op cursor from one page. A page that makes no progress is
/// reported as finished even if the server claimed `has_more`, so a broken
/// server cannot spin the pull loop.
fn advance_cursor(
    cursor: i64,
    _page_next: i64,
    last_seq: Option<i64>,
    has_more: bool,
) -> (i64, bool) {
    // Never trust a cursor beyond the entries actually delivered: doing so
    // would silently skip the intervening operations on the following page.
    let next = last_seq.unwrap_or(cursor).max(cursor);
    if next <= cursor {
        (next, false)
    } else {
        (next, has_more)
    }
}

/// Read an error body with a hard cap. A hostile server must not make the
/// client buffer a multi-gigabyte "error"; truncation is enough for diagnosis.
async fn text_bounded(mut response: Response, limit: usize) -> String {
    let mut out = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        let remaining = limit.saturating_sub(out.len());
        if chunk.len() > remaining {
            out.extend_from_slice(&chunk[..remaining]);
            out.extend_from_slice(b"...");
            break;
        }
        out.extend_from_slice(&chunk);
        if out.len() == limit {
            break;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Read a response body with a hard cap, failing instead of allocating past it.
async fn bytes_bounded(
    mut response: Response,
    limit: usize,
    what: &'static str,
) -> Result<Vec<u8>, TransferError> {
    if let Some(length) = response.content_length()
        && length > limit as u64
    {
        return Err(TransferError::TooLarge { what, limit });
    }
    let mut out = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if out.len().saturating_add(chunk.len()) > limit {
            return Err(TransferError::TooLarge { what, limit });
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Turn a response into a typed value, or a status error.
async fn json<T: DeserializeOwned>(response: reqwest::Response) -> Result<T, TransferError> {
    let status = response.status();
    if !status.is_success() {
        let body = text_bounded(response, MAX_ERROR_BODY).await;
        return Err(TransferError::Status {
            status: status.as_u16(),
            body,
        });
    }
    let bytes = bytes_bounded(response, MAX_JSON_BODY, "JSON response").await?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_resolve_under_a_subpath_base() {
        // Behind space-elevator: https://host/app/omni-sync/
        let client = Client::new("https://host/app/omni-sync").unwrap();
        assert_eq!(
            client.endpoint("/api/v1/devices/enroll").unwrap().as_str(),
            "https://host/app/omni-sync/api/v1/devices/enroll"
        );
    }

    #[test]
    fn endpoints_resolve_at_a_root_domain() {
        let client = Client::new("https://sync.example.com").unwrap();
        assert_eq!(
            client.endpoint("/api/v1/chunks/abc").unwrap().as_str(),
            "https://sync.example.com/api/v1/chunks/abc"
        );
        // A trailing slash on the base is equivalent.
        let client = Client::new("https://sync.example.com/").unwrap();
        assert_eq!(
            client.endpoint("/healthz").unwrap().as_str(),
            "https://sync.example.com/healthz"
        );
    }

    #[test]
    fn an_sse_data_frame_yields_its_head() {
        let mut buffer = b"event: head\ndata: 42\n\n".to_vec();
        assert!(matches!(take_frame(&mut buffer), Frame::Ready(Some(42))));
        assert!(buffer.is_empty());
    }

    #[test]
    fn a_keep_alive_frame_carries_no_head() {
        let mut buffer = b": keep-alive\n\n".to_vec();
        assert!(matches!(take_frame(&mut buffer), Frame::Ready(None)));
        assert!(buffer.is_empty());
    }

    #[test]
    fn a_partial_frame_waits_for_more_bytes() {
        let mut buffer = b"data: 7\n".to_vec();
        assert!(matches!(take_frame(&mut buffer), Frame::Incomplete));
        buffer.extend_from_slice(b"\n");
        assert!(matches!(take_frame(&mut buffer), Frame::Ready(Some(7))));
    }

    #[test]
    fn crlf_frames_and_several_frames_parse_in_order() {
        let mut buffer = b"data: 1\r\n\r\ndata: 2\r\n\r\n".to_vec();
        assert!(matches!(take_frame(&mut buffer), Frame::Ready(Some(1))));
        assert!(matches!(take_frame(&mut buffer), Frame::Ready(Some(2))));
        assert!(buffer.is_empty());
    }

    #[test]
    fn oversized_or_unterminated_sse_frames_are_refused() {
        assert!(!frame_too_large(b"data: 1\n\n"));
        let mut huge = vec![b'x'; MAX_SSE_FRAME_BYTES + 1];
        assert!(frame_too_large(&huge));
        // A terminator before the cap is a small frame even if the buffer also
        // holds later bytes.
        huge.extend_from_slice(b"\n\n");
        assert!(frame_too_large(&huge));
        let small = b"data: 1\n\ntrailing".to_vec();
        assert!(!frame_too_large(&small));
    }

    #[test]
    fn the_pagination_cursor_advances_and_never_spins() {
        // Old server: whole tail, next absent (0), has_more false.
        assert_eq!(advance_cursor(5, 0, Some(7), false), (7, false));
        // Normal page.
        assert_eq!(advance_cursor(5, 10, Some(10), true), (10, true));
        // Empty page at the current cursor.
        assert_eq!(advance_cursor(5, 5, None, false), (5, false));
        // A broken server claiming more without progress stops the loop.
        assert_eq!(advance_cursor(5, 5, Some(5), true), (5, false));
        // A page that goes backwards cannot move the cursor backwards.
        assert_eq!(advance_cursor(9, 3, Some(3), true), (9, false));
        assert_eq!(advance_cursor(5, 999, Some(7), true), (7, true));
        assert_eq!(advance_cursor(5, 999, None, true), (5, false));
    }
}
