-- agent_0002_holds.sql — per-path hold reasons for on_close and process holds.
--
-- `file_state.held_reason` records why an already-synced path is waiting;
-- `held_files` also covers brand-new files that are held before their first
-- upload, without polluting `file_state` (the last-synced manifest).

ALTER TABLE file_state ADD COLUMN held_reason TEXT;

CREATE TABLE held_files (
    folder_id   TEXT NOT NULL,
    path        BLOB NOT NULL,
    held_reason TEXT NOT NULL,
    PRIMARY KEY (folder_id, path)
);
