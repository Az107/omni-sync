//! HTTP client for the server's `/api/v1`.
//!
//! The daemon's entire interaction with the network goes through this one type,
//! so the sync engine can be reasoned about without reading HTTP code, and the
//! client can be pointed at a test server.

use omni_netproto::{
    ChunkPutResponse, CreateFolderRequest, EnrollRequest, EnrollResponse, FolderDto,
    FoldersResponse, ManifestResponse, MembersResponse, OpDto, OpsResponse, PROTOCOL_VERSION,
    RegisterMemberRequest, SubmitOpsRequest, SubmitOpsResponse,
};
use reqwest::{RequestBuilder, StatusCode, Url};
use serde::de::DeserializeOwned;

/// Everything that can go wrong talking to the server.
#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("invalid server url: {0}")]
    Url(String),
    #[error("server refused the request ({status}): {body}")]
    Status { status: u16, body: String },
}

/// A client for one server. The bearer token is attached to every call once set.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
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
        Ok(Self {
            http: reqwest::Client::new(),
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
        let url = self.endpoint("/api/v1/folders")?;
        let request = self.auth(self.http.post(url)).json(&CreateFolderRequest {
            name: name.to_owned(),
        });
        json(request.send().await?).await
    }

    pub async fn list_folders(&self) -> Result<Vec<FolderDto>, TransferError> {
        let url = self.endpoint("/api/v1/folders")?;
        let response: FoldersResponse = json(self.auth(self.http.get(url)).send().await?).await?;
        Ok(response.folders)
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
            let body = response.text().await.unwrap_or_default();
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

    pub async fn ops_since(
        &self,
        folder_id: &str,
        since: i64,
    ) -> Result<OpsResponse, TransferError> {
        let url = self.endpoint(&format!("/api/v1/folders/{folder_id}/ops?since={since}"))?;
        json(self.auth(self.http.get(url)).send().await?).await
    }

    /// Upload a chunk. `false` means the server already had it.
    pub async fn put_chunk(&self, hash: &str, bytes: Vec<u8>) -> Result<bool, TransferError> {
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
            let body = response.text().await.unwrap_or_default();
            return Err(TransferError::Status {
                status: status.as_u16(),
                body,
            });
        }
        Ok(Some(response.bytes().await?.to_vec()))
    }

    /// Open the server-sent event stream for a folder's op log.
    ///
    /// An event only says *that* the log grew; the ops themselves still come
    /// from [`Client::ops_since`], so the diff logic lives in one place.
    pub async fn stream_ops(&self, folder_id: &str) -> Result<OpStream, TransferError> {
        let url = self.endpoint(&format!("/api/v1/folders/{folder_id}/ops/stream"))?;
        let response = self.auth(self.http.get(url)).send().await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
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

/// Turn a response into a typed value, or a status error.
async fn json<T: DeserializeOwned>(response: reqwest::Response) -> Result<T, TransferError> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(TransferError::Status {
            status: status.as_u16(),
            body,
        });
    }
    Ok(response.json::<T>().await?)
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
}
