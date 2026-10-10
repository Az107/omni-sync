//! HTTP client for the server's `/api/v1`.
//!
//! Only the enrollment call exists so far — the ops/chunk endpoints arrive with
//! M2/M3. Keeping the client here (rather than inside `omni-agent`) means the
//! daemon's networking is testable against this one seam.

use omni_netproto::{EnrollRequest, EnrollResponse, PROTOCOL_VERSION};
use reqwest::Url;

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

/// A client for one server. The bearer token is attached per call once set.
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

    /// Exchange a one-time pairing code for a device id and token.
    pub async fn enroll(&self, name: &str, code: &str) -> Result<EnrollResponse, TransferError> {
        let url = self.endpoint("/api/v1/devices/enroll")?;
        let request = EnrollRequest {
            code: code.to_owned(),
            name: name.to_owned(),
            protocol_version: PROTOCOL_VERSION,
        };
        let response = self.http.post(url).json(&request).send().await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(TransferError::Status {
                status: status.as_u16(),
                body,
            });
        }
        Ok(response.json::<EnrollResponse>().await?)
    }
}
