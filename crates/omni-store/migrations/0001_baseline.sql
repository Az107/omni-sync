-- 0001_baseline.sql — the server schema from plan.md.
--
-- Paths are BLOB, not TEXT: an OS path is bytes. Hashes are lowercase hex.
-- Timestamps are fixed-width RFC 3339 (UTC, milliseconds, trailing Z) written
-- with strftime so they compare correctly as text.

CREATE TABLE devices (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    pubkey       BLOB NOT NULL DEFAULT x'',
    token_hash   BLOB NOT NULL,
    enrolled_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    last_seen_at TEXT,
    revoked_at   TEXT
);
CREATE UNIQUE INDEX devices_token_hash ON devices (token_hash);

CREATE TABLE pairing_codes (
    code               TEXT PRIMARY KEY,
    created_at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    expires_at         TEXT NOT NULL,
    consumed_at        TEXT,
    consumed_device_id TEXT
);

CREATE TABLE folders (
    id                 TEXT PRIMARY KEY,
    name               TEXT NOT NULL,
    retention_versions INTEGER NOT NULL DEFAULT 50,
    retention_days     INTEGER NOT NULL DEFAULT 90,
    encrypted          INTEGER NOT NULL DEFAULT 0,
    created_at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE members (
    folder_id  TEXT NOT NULL REFERENCES folders (id) ON DELETE CASCADE,
    device_id  TEXT NOT NULL REFERENCES devices (id) ON DELETE CASCADE,
    root_path  BLOB NOT NULL DEFAULT x'',
    mode       TEXT NOT NULL DEFAULT 'both',
    acked_seq  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (folder_id, device_id)
);

-- The append-only op log. The server is the only writer of `id`.
CREATE TABLE ops (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    folder_id  TEXT NOT NULL REFERENCES folders (id) ON DELETE CASCADE,
    device_id  TEXT NOT NULL,
    base_seq   INTEGER NOT NULL,
    kind       TEXT NOT NULL,
    path       BLOB NOT NULL,
    payload    TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX ops_folder_id ON ops (folder_id, id);

-- Every version a path has ever had. Last-write-wins supersedes; it never deletes.
CREATE TABLE versions (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    folder_id       TEXT NOT NULL REFERENCES folders (id) ON DELETE CASCADE,
    path            BLOB NOT NULL,
    device_id       TEXT NOT NULL,
    seq             INTEGER NOT NULL,
    base_version_id INTEGER,
    size            INTEGER NOT NULL DEFAULT 0,
    mtime_ns        INTEGER NOT NULL DEFAULT 0,
    mode            INTEGER NOT NULL DEFAULT 0,
    chunks_json     TEXT NOT NULL DEFAULT '[]',
    restored_from   INTEGER,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX versions_folder_path ON versions (folder_id, path, id);

CREATE TABLE files (
    folder_id         TEXT NOT NULL REFERENCES folders (id) ON DELETE CASCADE,
    path              BLOB NOT NULL,
    active_version_id INTEGER REFERENCES versions (id),
    deleted_at        TEXT,
    PRIMARY KEY (folder_id, path)
);

-- The dashboard review queue: a version that was applied over another.
CREATE TABLE overwrites (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    folder_id         TEXT NOT NULL,
    path              BLOB NOT NULL,
    losing_version_id  INTEGER NOT NULL,
    winning_version_id INTEGER NOT NULL,
    resolved_at       TEXT,
    seen_at           TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX overwrites_open ON overwrites (resolved_at, id);

-- Content-addressed chunk index. The bytes live under data_dir/chunks/.
CREATE TABLE chunks (
    hash      TEXT PRIMARY KEY,
    size      INTEGER NOT NULL,
    refs      INTEGER NOT NULL DEFAULT 0,
    stored_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    folder_id  TEXT,
    device_id  TEXT,
    kind       TEXT NOT NULL,
    detail     TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX events_recent ON events (id);
