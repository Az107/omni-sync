-- 0003_folder_settings.sql
--
-- Per-folder sync policy, shared by every client and the server. A vault needs
-- excludes (`.obsidian/workspace.json`, `.trash/`, plugin data) and a
-- permission policy: a JavaScript client can neither read nor write POSIX mode
-- bits, and `FileMeta::equivalent` compares mode, so without a shared policy a
-- browser-based client and a unix daemon would push each other's files forever.
--
-- `exclude` is a JSON array of path patterns matched by `omni_core::ignore`.
-- `preserve_permissions = 0` makes every client report `mode = 0` (and skip
-- chmod on apply), which compares equal across platforms.

ALTER TABLE folders ADD COLUMN exclude TEXT NOT NULL DEFAULT '[]';
ALTER TABLE folders ADD COLUMN preserve_permissions INTEGER NOT NULL DEFAULT 1;
