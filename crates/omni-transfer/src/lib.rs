//! HTTP client for the server's `/api/v1`.
//!
//! The daemon's entire interaction with the network goes through this one type,
//! so the sync engine can be reasoned about without reading HTTP code, and the
//! client can be pointed at a test server.

use omni_netproto::{
    ChunkPutResponse, CreateFolderRequest, EnrollRequest, EnrollResponse, FolderDto,
    FoldersResponse, ManifestResponse, OpDto, OpsResponse, PROTOCOL_VERSION, SubmitOpsRequest,
    SubmitOpsResponse,
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
        let base = Url::parse(base_url).map_err(|e| TransferError::Url(e.to_string()))?;
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
        self.base
            .join(path)
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
