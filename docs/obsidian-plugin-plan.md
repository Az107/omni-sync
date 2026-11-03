# Obsidian client — plan

An Obsidian community plugin that makes a vault a first-class omni-sync
client, on desktop **and mobile**. Mobile is the constraint that decides the
architecture: Obsidian mobile plugins run in a WebView and cannot spawn a
process, so the plugin cannot supervise `omni-sync daemon`. It has to speak
the server's protocol itself.

## Decision

- **Native TypeScript client** inside the plugin, against the existing
  `ops` + `chunks` API. No new file-level sync API: a note is under the
  256 KiB average chunk anyway, so a per-file endpoint buys nothing for
  markdown and loses delta sync for attachments. It would also add a second
  path that mints versions on the server, which is the drift this project
  avoids.
- Transport is Obsidian's `requestUrl` (main process, no CORS, works on
  mobile), plus the server's SSE stream later. v1 polls `ops?since` every
  5 s while the app is visible and syncs immediately on foreground.
- The plugin lives in its own repo (`omni-sync-obsidian`), released with
  `main.js` / `manifest.json` / `styles.css` on `obsidian-v*` tags for BRAT.
  The community-store layout requires `manifest.json` at the repo root.
- Server changes stay additive and optional, so `PROTOCOL_VERSION` remains
  `1`.

## Why Phase 0 comes first

Two things in the current tree make a vault client impossible to do
correctly, and both are bugs independent of Obsidian:

1. **No exclusion support.** `scan::is_ignored` filters basenames only
   (`.DS_Store`, `*.part`, `*.omni-tmp`); `.omniignore` is skipped as a file
   but never parsed; `exclude` appears in `plan.md` and nowhere in the code.
   A vault therefore syncs `.obsidian/workspace.json` (per-device layout
   churn), `.obsidian/cache/`, `.trash/`, and the plugin's `data.json`.
2. **`FileMeta::equivalent` compares `mode`.** A JS client cannot read or
   write POSIX bits, and Windows already pins `0o644` while unix sends real
   bits. A plugin and a desktop daemon on one folder would ping-pong forever
   on every `0600` or `0755` file.

Both must be *folder-level* (server-side) properties. Per-device config
means two clients disagree about the same folder.

## Phase 0 — contract and Rust client parity

Deliverables in this repo:

| area | change |
|---|---|
| migration | `0003_folder_settings.sql`: `folders.exclude` (JSON array), `folders.preserve_permissions` |
| `omni-store::folders` | load/save the two fields, `update_settings`; `Folder::new` |
| `omni-netproto` | `FolderDto.exclude`, `FolderDto.preserve_permissions`, `CreateFolderRequest.exclude`, `UpdateFolderRequest`, `OverwriteDto.folder_id`; all `#[serde(default)]` |
| `omni-server` | `PATCH /api/v1/folders/{id}`; create accepts the preset; dashboard shows settings; overwrite DTO carries the folder |
| `omni-core::ignore` | pure byte-path glob matcher (`*`, `**`, `?`, trailing `/`, no-slash = any segment), with a shared JSON test corpus |
| `omni-agent` | `EngineConfig.exclude` + `preserve_permissions`; `scan` filters; `apply` skips chmod when `mode == 0` |
| `omni-agent` (trap) | `push_inner` **filters the loaded `last` manifest through the matcher** before diffing, or a newly excluded path looks deleted and emits a `Delete` |
| `omni-agent` (trap) | `pull_inner` skips excluded paths but still advances `applied_seq` |
| `omni-cli` | `WatchEntry` carries the folder settings; `add`/`daemon` refresh them from the server; `folder create --vault` preset |
| chunker | committed golden boundary vectors for the TypeScript port |
| docs | AGENTS.md invariants; `plan.md` excludes become real |

Acceptance: workspace tests and clippy clean; a test proves that excluding a
previously-synced path produces no `Delete` in either direction.

## Phase 1 — plugin skeleton, pairing, pull-only

New repo. Pull-only on purpose: it proves transport, chunker, apply and
exclusion against an empty vault without touching the push path.

- scaffold: TypeScript, esbuild, vitest, `isDesktopOnly: false`
- `api.ts` — DTOs, `requestUrl`, bearer auth, sub-path base URLs, backoff
- `chunker.ts` — gear table + rolling hash (32-bit is exact: only the low
  `avg-1` bits are tested), SHA-256 via `crypto.subtle`, golden vectors
- `ignore.ts` — port of `omni-core::ignore`, same JSON corpus
- `state.ts` — IndexedDB per vault: token, `applied_seq`, per-path
  `{version_id, size, mtime_ns, chunks}`. The token never lives in the vault
- `pull.ts` — manifest, `ops?since`, verified chunk fetch, atomic apply
  (`.omni-tmp` + rename, `mkdir` parents), first-sync adoption
- `onboarding.ts` — server URL + pairing code → enroll → pick/create folder
  with the vault preset → register member → first sync
- status bar, "Sync now" command, settings tab, pause

Acceptance: a file created by a Rust client appears byte-identical
(including a binary attachment) in an empty vault; `.obsidian/workspace.json`
never transfers; re-pulling the same head writes nothing.

## Phase 2 — push and convergence

Port the engine invariants in order, each with a test:

1. `applied_seq == 0` pulls first; otherwise push then pull
2. batched `submit_ops`, `advance_watermark` over the contiguous run only
3. a pull immediately after the last push batch (global `ops.id` gaps)
4. echo skip: same `version_id` and the file exists → advance, don't fetch
5. first-sync adoption: equivalent → adopt; different → synthetic baseline
   `version_id = 0`, push, let the server record the overwrite

Obsidian specifics:

- debounce vault events 500 ms; targeted work per event, full scan on
  foreground and every 5 min (10k `adapter.stat` calls per keystroke is
  unusable on mobile)
- self-write suppression + post-write mtime
- `rename` becomes delete + put (the server refuses `Move`)
- defer applies for files open in an editor; re-check mtime before a
  deferred write

Acceptance: the AGENTS.md two-client recipe with one Rust daemon and the
plugin, including offline edits on both sides converging with one overwrite
row and an idle vault making no requests but the poll.

## Phase 3 — UX, conflicts, lifecycle

Status bar, history + restore, overwrite review, deep link to the dashboard,
mobile foreground/background lifecycle, refusal when Obsidian Sync is
configured, warnings for iCloud/Dropbox vaults and case-insensitive or
non-UTF-8 path collisions.

## Phase 4 — hardening

10k files / 1 GB profile, bounded chunk concurrency, cancellation, iOS and
Android attachment round trips, BRAT release pipeline, README.

## Invariants the TypeScript engine must mirror

From AGENTS.md "Engine & Watcher Invariants", all of them:

1. mutations only drive push; discarded access events never wake a scan
2. sync order depends on `applied_seq`
3. global `ops.id`, contiguous watermark, post-push pull, echo skip
4. never two engines on one tree (a plugin and a daemon on the same vault)
5. the SSE waker must die with the engine (polling in v1)
6. atomic config writes (plugin state, not TOML)
7. logs never on stdout (n/a)

## Progress

- **Phase 0 — done.** Server migration `0003` adds `folders.exclude` and
  `folders.preserve_permissions`; `FolderDto` carries them; `GET`/`PATCH
  /api/v1/folders/{id}` and a dashboard policy form edit them;
  `omni_core::Ignore` is the shared matcher; the agent filters the push
  baseline, skips-and-advances excluded pulls, and honours the `mode = 0`
  sentinel; `omni-sync folder create <name> --vault` writes the preset.
  119 Rust tests, clippy clean. Verified live: a vault folder shared between
  two enrolled clients synced only its notes, ignored `.obsidian/`, `.trash/`
  and an excluded script, and adding an exclude produced **no** `Delete`.
- **Phase 1 — done (pull only).** The plugin repo has transport, verified
  chunk apply, IndexedDB state, pairing, status bar and settings. Its chunker
  and ignore matcher are asserted against the Rust-generated vectors (80
  tests). A pull never overwrites a saved local edit: it keeps the file and
  records the synthetic baseline phase 2 needs.
- **Phase 2 — done.** Push on vault events with a 500 ms debounce and a
  targeted scan, chunk upload, batched `submit_ops`, the contiguous
  watermark, a pull immediately after each push, echo suppression, rename as
  delete + put, held-back remote updates for open editors (persisted), and
  dirty-file deferral so the push wins. `FileMeta::equivalent` now treats
  `mode = 0` as a wildcard, so a client without POSIX modes can push into any
  folder. Verified by 93 unit tests plus an opt-in interop test that drives
  the real `Api`/`Pusher`/`Puller` against a live server: text + a 300 KB
  binary, an edit, a delete, a rename, and an offline conflict that produced
  exactly one overwrite row with the right folder id.
- **Phase 3 — done.** Per-file version history and restore (a `Resolve` op, so
  every client converges), a conflict review modal with *restore the
  superseded version* and *keep active*, a conflict count in the status bar
  with a notice when one appears, a dashboard deep link, and vault safety
  checks (Obsidian Sync enabled, a cloud-synced vault, paths that collide on a
  case-insensitive disk). Server additions: `OverwriteDto.id` and
  `POST /api/v1/overwrites/{id}/dismiss`, so the plugin can accept the active
  version without the dashboard. 104 plugin tests + the live interop run.
- **Phase 4 — done, with one caveat.** The op log is paged and a new vault is
  seeded from `head` + `manifest` instead of replaying history; chunk
  uploads/downloads run four at a time; a size cap keeps a phone from reading a
  huge attachment; polling backs off and a revoked token stops retrying; a sync
  can be cancelled between files; the SSE stream is used when the platform can
  stream, with the poll as fallback (the server sends CORS headers for it);
  deleted files are restorable from a trash view; excludes and the device name
  are editable in the plugin; unpair revokes the token. CI, a release workflow,
  ESLint/Prettier and a version-bump script are in place. **Caveat:** none of
  this has run on a real iOS/Android device yet, and DTOs are still
  hand-written rather than generated from `omni-netproto`.

## Risks

- Editor clobbering is the main UX risk. Deferral is a heuristic; the worst
  case is a recoverable overwrite row, which is the designed failure mode.
- The `.obsidian/` policy is opinionated. Default excludes are volatile
  files only; users can opt into syncing more.
- NFC/NFD and case-insensitive disks can duplicate or collide notes between
  a Linux daemon, macOS and iOS. Detect and warn; never rewrite bytes.
- Do not run the plugin and the desktop daemon on the same vault.
