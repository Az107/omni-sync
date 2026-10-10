-- agent_0001.sql — the client-side database.
--
-- `file_state` is the *last synced* view: what this device believes is
-- committed on the server. The diff between it and the on-disk scan is what to
-- push; the diff between it and the server's manifest is what to pull.
--
-- Paths are BLOB, as everywhere. `chunks_json` is the serialized chunk list.

CREATE TABLE file_state (
    folder_id   TEXT NOT NULL,
    path        BLOB NOT NULL,
    size        INTEGER NOT NULL,
    mtime_ns    INTEGER NOT NULL,
    mode        INTEGER NOT NULL,
    chunks_json TEXT NOT NULL DEFAULT '[]',
    -- The server version this content corresponds to. This is what the client
    -- sends back as base_version_id when it edits the path.
    version_id  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (folder_id, path)
);

CREATE TABLE sync_state (
    folder_id   TEXT PRIMARY KEY,
    applied_seq INTEGER NOT NULL DEFAULT 0
);
