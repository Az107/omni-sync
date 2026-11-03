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
    /// One entry per submitted op, in request order.
    pub applied: Vec<AppliedOpDto>,
    /// Overwrite review entries raised by this batch, if any.
    #[serde(default)]
    pub overwrites: Vec<OverwriteDto>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppliedOpDto {
    pub seq: i64,
    pub version_id: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolveRequest {
    pub path: String,
    /// The version whose content should become active again.
    pub restored_from: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryEntryDto {
    pub version_id: i64,
    pub seq: i64,
    pub device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version_id: Option<i64>,
    pub size: u64,
    pub mtime_ns: i64,
    pub mode: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored_from: Option<i64>,
    pub created_at: String,
    #[serde(default)]
    pub active: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryResponse {
    pub entries: Vec<HistoryEntryDto>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OverwriteDto {
    /// The review row id, for dismissing it. Absent on the inline rows that
    /// `submit_ops` returns (they have no row id yet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    pub path: String,
    pub losing_version_id: i64,
    pub winning_version_id: i64,
    /// Which folder the path belongs to. Optional so a client talking to an
    /// older server still deserializes; an empty/None value means unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<String>,
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
    /// The seq of the last entry in this page, or the request's `since` when
    /// the page is empty — the cursor for the next `ops?since=` call.
    /// Defaults to 0 so a client built against the un-paginated shape (and an
    /// older server that never sent it) still parses.
    #[serde(default)]
    pub next: i64,
    /// Whether an op with `seq > next` exists. Ignored by the Rust client,
    /// which pulls until a page comes back empty; the paginated browser/WebView
    /// client uses it to know when to stop.
    #[serde(default)]
    pub has_more: bool,
}

/// The current op-log head of a folder (`GET /api/v1/folders/{id}/head`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HeadResponse {
    /// The highest op id in the folder's log; 0 when the log is empty.
    pub head: i64,
}

/// One path in a folder whose file row is marked deleted.
///
/// `version_id` is the most recent version with `size > 0` — the last content
/// the path had before it was deleted. Restoring is the existing
/// `POST /api/v1/folders/{id}/resolve` with that id, so the trash view needs no
/// new restore route.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeletedFileDto {
    /// Base64 of the raw path bytes, like every other path on the wire.
    pub path: String,
    pub version_id: i64,
    pub size: u64,
    /// RFC 3339 UTC, milliseconds, trailing `Z` (when the delete was applied).
    pub deleted_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeletedFilesResponse {
    /// Newest deletion first.
    pub files: Vec<DeletedFileDto>,
}

/// A device as the API reports it to that device (`/api/v1/devices/me`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceDto {
    pub id: String,
    pub name: String,
}

/// `PATCH /api/v1/devices/me`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RenameDeviceRequest {
    /// A non-empty label for the device ("laptop", "desktop"). Whitespace only
    /// is rejected with 400.
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateFolderRequest {
    pub name: String,
    /// See [`FolderDto::exclude`]. Omitted by older clients.
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default = "default_true")]
    pub preserve_permissions: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateFolderRequest {
    /// Replace the exclude list. `None` leaves it unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
    /// `None` leaves the policy unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preserve_permissions: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FolderDto {
    pub id: String,
    pub name: String,
    /// Path patterns every client must ignore in this folder. A folder
    /// property rather than a device one, so two clients cannot disagree.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// When false, clients report `mode = 0` and do not touch permissions on
    /// apply. Required for clients that cannot express POSIX modes (mobile),
    /// and for `FileMeta::equivalent` to agree across platforms.
    #[serde(default = "default_true")]
    pub preserve_permissions: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FoldersResponse {
    pub folders: Vec<FolderDto>,
}

/// A device telling the server which folder it holds and where. `root_path`
/// is base64 of raw path bytes, like every other path on the wire.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegisterMemberRequest {
    pub root_path: String,
    #[serde(default = "default_member_mode")]
    pub mode: String,
}

fn default_member_mode() -> String {
    "both".to_string()
}

/// One folder membership, as the server reports it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemberDto {
    pub device_id: String,
    pub device_name: String,
    pub root_path: String,
    pub mode: String,
    pub acked_seq: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MembersResponse {
    pub members: Vec<MemberDto>,
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

    #[test]
    fn a_register_member_defaults_to_both() {
        let request: RegisterMemberRequest =
            serde_json::from_str(r#"{"root_path":"L2hvbWUvbWUvRG9jcw=="}"#).unwrap();
        assert_eq!(request.mode, "both");
        assert_eq!(request.root_path, "L2hvbWUvbWUvRG9jcw==");
    }

    #[test]
    fn an_old_folder_dto_keeps_permissions_and_has_no_excludes() {
        // A server from before folder settings only sends id + name.
        let folder: FolderDto = serde_json::from_str(r#"{"id":"f1","name":"Vault"}"#).unwrap();
        assert_eq!(folder.exclude, Vec::<String>::new());
        assert!(folder.preserve_permissions);
    }

    #[test]
    fn a_create_request_defaults_to_server_settings() {
        let request: CreateFolderRequest = serde_json::from_str(r#"{"name":"Vault"}"#).unwrap();
        assert!(request.exclude.is_empty());
        assert!(request.preserve_permissions);
    }

    #[test]
    fn the_obsidian_client_op_shape_deserializes() {
        // Exactly what `src/push.ts` submits: `mode` is the 0 sentinel,
        // `base_version_id` is null for a create, and the path is base64.
        let json = r#"{
            "ops": [{
                "base_seq": 3,
                "base_version_id": null,
                "kind": "put",
                "path": "bm90ZXMvdG9kby5tZA==",
                "size": 8,
                "mtime_ns": 1700000000000000000,
                "mode": 0,
                "chunks": [{"hash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", "len": 8}]
            }]
        }"#;
        let request: SubmitOpsRequest = serde_json::from_str(json).unwrap();
        let op = &request.ops[0];
        assert_eq!(op.base_version_id, None);
        assert_eq!(op.mode, 0);
        assert_eq!(decode_path(&op.path).unwrap(), b"notes/todo.md");
        assert_eq!(op.chunks.len(), 1);
    }

    #[test]
    fn an_old_ops_response_still_parses_without_pagination() {
        // The payload a server from before pagination sends: entries only.
        let response: OpsResponse = serde_json::from_str(r#"{"entries":[]}"#).unwrap();
        assert!(response.entries.is_empty());
        assert_eq!(response.next, 0);
        assert!(!response.has_more);
    }

    #[test]
    fn an_ops_response_carries_its_page_cursor() {
        let response = OpsResponse {
            entries: Vec::new(),
            next: 42,
            has_more: true,
        };
        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(json, r#"{"entries":[],"next":42,"has_more":true}"#);
        let back: OpsResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back, response);
    }

    #[test]
    fn the_trash_and_device_self_dtos_round_trip() {
        let deleted = DeletedFilesResponse {
            files: vec![DeletedFileDto {
                path: encode_path(b"notes/old.md"),
                version_id: 7,
                size: 12,
                deleted_at: "2026-11-03T10:00:00.000Z".into(),
            }],
        };
        let json = serde_json::to_string(&deleted).unwrap();
        assert_eq!(
            json,
            r#"{"files":[{"path":"bm90ZXMvb2xkLm1k","version_id":7,"size":12,"deleted_at":"2026-11-03T10:00:00.000Z"}]}"#
        );
        let back: DeletedFilesResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back, deleted);

        let head: HeadResponse = serde_json::from_str(r#"{"head":9}"#).unwrap();
        assert_eq!(head.head, 9);

        let rename: RenameDeviceRequest = serde_json::from_str(r#"{"name":"laptop"}"#).unwrap();
        assert_eq!(rename.name, "laptop");
        let device: DeviceDto = serde_json::from_str(r#"{"id":"d1","name":"laptop"}"#).unwrap();
        assert_eq!(device.id, "d1");
        assert_eq!(device.name, "laptop");
    }
}
