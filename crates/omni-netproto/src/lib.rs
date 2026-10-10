//! The wire contract between clients and the server (`/api/v1`).
//!
//! These types are the JSON the API actually parses and emits; they are
//! deliberately separate from `omni-core`'s domain types so a wire change is a
//! visible edit here rather than an accidental `derive` on a domain struct.
//!
//! Paths cross the wire base64-encoded. JSON strings cannot carry arbitrary
//! bytes, and a Linux filename is not guaranteed to be UTF-8; base64 is the
//! lossless transport for the `PathBytes` the core speaks.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};

mod convert;
pub use convert::{decode_chunks, decode_file_meta, encode_chunks, entry_path};

/// Bumped when the wire shape changes incompatibly. `Hello`/`enroll` carry it.
pub const PROTOCOL_VERSION: u32 = 1;

/// Encode a raw path for the wire.
pub fn encode_path(path: &[u8]) -> String {
    B64.encode(path)
}

/// Decode a wire path back to raw bytes.
pub fn decode_path(encoded: &str) -> Result<Vec<u8>, base64::DecodeError> {
    B64.decode(encoded)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnrollRequest {
    /// The one-time code shown in the dashboard.
    pub code: String,
    /// A human label for the device ("laptop", "desktop").
    pub name: String,
    pub protocol_version: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnrollResponse {
    pub device_id: String,
    /// Bearer token for every subsequent call. Shown once.
    pub token: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChunkRefDto {
    pub hash: String,
    pub len: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OpKindDto {
    Put,
    Delete,
    Move,
    Resolve,
}

/// One op as submitted. `path` and `from` are base64 of raw path bytes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpDto {
    #[serde(default)]
    pub base_seq: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version_id: Option<i64>,
    pub kind: OpKindDto,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub mtime_ns: i64,
    #[serde(default)]
    pub mode: u32,
    #[serde(default)]
    pub chunks: Vec<ChunkRefDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored_from: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubmitOpsRequest {
    pub ops: Vec<OpDto>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubmitOpsResponse {
    /// The op id the server assigned to the last op in the batch.
    pub seq: i64,
    /// Overwrite review entries raised by this batch, if any.
    #[serde(default)]
    pub overwrites: Vec<OverwriteDto>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolveRequest {
    pub path: String,
    /// The version whose content should become active again.
    pub restored_from: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OverwriteDto {
    pub path: String,
    pub losing_version_id: i64,
    pub winning_version_id: i64,
}

/// One file's current state, as the server reports it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestEntryDto {
    pub path: String,
    pub version_id: i64,
    pub size: u64,
    pub mtime_ns: i64,
    pub mode: u32,
    pub chunks: Vec<ChunkRefDto>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestResponse {
    pub entries: Vec<ManifestEntryDto>,
}

/// One entry of the op log as a client pulls it: the op *and* the content it
/// produced, so applying never needs a second lookup.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogEntryDto {
    pub seq: i64,
    #[serde(default)]
    pub version_id: i64,
    pub kind: OpKindDto,
    pub path: String,
    pub device_id: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub mtime_ns: i64,
    #[serde(default)]
    pub mode: u32,
    #[serde(default)]
    pub chunks: Vec<ChunkRefDto>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpsResponse {
    pub entries: Vec<LogEntryDto>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateFolderRequest {
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FolderDto {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FoldersResponse {
    pub folders: Vec<FolderDto>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChunkPutResponse {
    /// True when the bytes were newly stored, false when already present.
    pub created: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthResponse {
    pub ok: bool,
    pub version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_round_trip_through_base64_losslessly() {
        // A path that is valid UTF-8 and one that is not.
        for raw in [&b"notes/todo.md"[..], &[0x72, 0xFF, 0xFE, 0x00, 0x0A][..]] {
            let encoded = encode_path(raw);
            assert_eq!(decode_path(&encoded).unwrap(), raw);
        }
    }

    #[test]
    fn enroll_round_trips_as_json() {
        let req = EnrollRequest {
            code: "ABCD-1234".into(),
            name: "laptop".into(),
            protocol_version: PROTOCOL_VERSION,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: EnrollRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }
}
