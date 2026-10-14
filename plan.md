# omni-sync — automatic file sync: server + dashboard + clients

Design for a personal sync service with two parts. Mirrors `imp` on the
code side (Rust workspace, edition 2024, Rust 1.89, crates with I/O at
the edges) and `space-elevator` on the deploy side (VPS, Podman,
Traefik + Let's Encrypt, systemd user unit).

## Status (2026-10-10)

**M0–M4 done — the MVP syncs and is manageable.** A server and two
daemons were run against each other: create (a text and a binary file,
byte-identical on both sides), bidirectional edit, delete propagation,
and an offline edit on both devices yielding exactly one overwrite row
with last-write-wins converging both sides. The dashboard then restored
the superseded version and both sides converged again. 47 tests, clippy
clean. See `AGENTS.md` for entry points, the smoke test and the
two-client verification recipe.

Still open: **M5** (client `history`/`resolve`/`flush` commands, chunk
GC, membership enforcement, `notify`-driven wakeup, TLS deploy to the
VPS).

**M6 (onboarding + updates) is built** — see the M6 section below. The
dashboard mints a magic link, `omni-sync enroll --link` opens a loopback
listener and the browser hands it the code, `omni-sync add` is a wizard
that lists the server's folders and completes paths, and both binaries
ship `update`, which verifies a release against its `SHA256SUMS` before
installing. Folder membership is now recorded (`members`) and shown on
the dashboard; it is still not *enforced*.

## Answers folded in (2026-10-10)

1. **Rust workspace, two binaries.**
2. **Relay through the server** — all ops and chunks traverse the server,
   which is also the backup. Direct client↔client later, behind the
   transport interface.
3. **Public HTTPS immediately** — `sync.albruiz.dev` behind Traefik + LE
   from day one, so auth is day-one scope, not a later patch.
4. **Last-write-wins, but history is kept** — no blocking conflicts. The
   server records every version; the dashboard's history view resolves
   after the fact (rollback to any version, or restore either side of an
   overwrite). Overwrites are reported, never silently accepted.
5. **First folders: documents.**

## Goal

- **Server** (`omni-sync-server`): always-on coordinator. Registers
  devices, defines shared folders and which device holds which folder,
  sequences every change, keeps every version, relays chunks, and serves
  the dashboard where devices, folders, history and resolutions live.
- **Client** (`omni-sync`): daemon per device. Watches assigned folders
  and syncs the moment a file changes, with no manual step.

Same repo, same protocol, one workspace — protocol drift is a compile
error, not a runtime surprise.

## Decisions

1. **Server-authoritative op log.** The server sequences ops per folder;
  clients submit ops with the `base_seq` they acted on. This gives
  ordering, offline catch-up, rename/delete semantics and the version
  chain from one rule.
2. **Last-write-wins by server receipt, with full history.** When a
  client submits a version whose `base_version_id` is not the current
  active one, the server applies it anyway (newest wins) but records an
  **overwrite event** naming the superseded version. The dashboard shows
  both, and can "resolve" afterwards:
  - *rollback to version N* — emits a new version (`restored_from = N`)
    whose chunk list is version N's, so every device converges on the old
    content by the ordinary sync path;
  - *restore either side* — same mechanism with the other version's id.
  Nothing is ever deleted by LWW; retention does the cleanup, and it is
  configurable (keep N versions / T days, floor of 1 backup per file).
3. **Content-defined chunking.** Gear-hash FastCDC, ~256 KiB avg
  (64 KiB–1 MiB, configurable). Chunks are content-addressed (`sha256`),
  deduped across all devices into one store, transferred streaming, and
  verified on arrival against the hash they claim.
4. **The server is the only sequencer.** Device clocks never order
  anything; `base_version_id` and the op id are the only ordering inputs.
5. **Public exposure means real auth.** Per-device tokens (Bearer, stored
  hashed, revocable, issued by one-time pairing codes), device keypairs
  (Ed25519) that sign ops, and a dashboard login. TLS terminates at
  Traefik; the server itself binds loopback in the container.
6. **Chunk encryption is per-folder and optional, not day-one.**
  Convergent encryption (plaintext-hash-derived key) keeps dedup alive
  if we ever want it; plaintext-at-rest on the VPS is accepted for v1
  and the folder key design is sketched below so it does not force a
  schema change later.
7. **Dashboard is server-rendered HTML over HTMX** — `askama` templates
  (`crates/omni-server/templates/`) + `rust-embed` static, served by the
  same axum app that serves `/api/v1`. No node toolchain, no build step,
  works from a phone. Tauri is explicitly **not** the dashboard: a web
  page is installable on nothing and reaches you anywhere, and a system
  webview would be a per-OS packaging problem on the VPS. A Tauri tray
  client (status, conflict popups) is a possible later add-on over the
  daemon, never the management UI.

## Architecture

```
  laptop ──┐                              ┌──────── server (VPS, always-on) ────────┐
  desktop ─┼── HTTPS /api/v1 ────────────▶│ devices · folders · membership          │
  phone ───┘   ops (JSON) + chunks        │ op log per folder (sequencer)          │
                                         │ version history (all versions kept)    │
                                         │ chunk store, deduped, refcounted, GC    │
                                         │ overwrite events → dashboard review     │
                                         │ HTMX dashboard / REST /api/v1           │
                                         └──────── Traefik + LE (sync.albruiz.dev) ┘
```

### Client sync loop

1. **Watch**: recursive `notify` watches per assigned root; events
   coalesce after 500 ms quiet (save bursts produce one op, not ten).
2. **Scan**: hash the changed file into a chunk list; record
   `path, size, mtime_ns, content_hash`.
3. **Push**: `POST /folders/{id}/ops` with `base_seq`; upload only the
   chunks the server lacks (`PUT /chunks/{hash}` → 409 "exists" = skip).
4. **Pull**: `GET /folders/{id}/ops?since=<applied_seq>` (2 s poll; SSE
   later). Apply in order, in the same transaction as `applied_seq`.
5. **Reconcile**: full walk every 5 min and at reconnect, to catch edits
   made while the daemon was down and any events the watcher dropped.

Ops: `put` (path, size, mtime, chunks[]), `delete` (path),
`move` (from, to), `resolve` (path, restored_from).

## Data model

Server SQLite (`~/.local/state/omni-sync/server.db`, WAL, single writer,
all I/O through `Store::blocking` like `imp-store`):

```sql
devices(id TEXT PK, name, pubkey, token_hash, enrolled_at, last_seen_at, revoked_at)
folders(id TEXT PK, name, retention_versions INT, retention_days INT,
        encrypted INT, created_at)
members(folder_id, device_id, root_path, mode /*send|recv|both*/,
        applied_seq INT, PK(folder_id, device_id))
ops(id INTEGER PK AUTOINCREMENT, folder_id, device_id, base_seq INT,
    kind, path, payload JSON, created_at)          -- append-only log
versions(id INTEGER PK AUTOINCREMENT, folder_id, path, device_id, seq,
         base_version_id, size, mtime_ns, mode, chunks_json,
         restored_from INTEGER NULL, created_at)   -- every version ever
files(folder_id, path, active_version_id, deleted_at, PK(folder_id, path))
overwrites(id INTEGER PK AUTOINCREMENT, folder_id, path, losing_version_id,
           winning_version_id, seen_at)            -- dashboard review queue
chunks(hash TEXT PK, size INT, refs INT, stored_at)
events(id INTEGER PK AUTOINCREMENT, folder_id, device_id, kind, detail JSON,
       created_at)
```

A rollout that needs "keep both" is still expressible (`restored_from` a
loser under a new path); it is just not the default, because the answer
was last-wins.

Client SQLite (`agent.db`) + chunk store `~/.local/share/omni-sync/chunks/<h[:2]>/<h>`:

```sql
assignments(folder_id, root_path, applied_seq, mode)
file_state(folder_id, path, size, mtime, content_hash, version_seq)
local_chunks(hash, size)
pending_ops(id INTEGER PK, folder_id, payload JSON)   -- replayed on reconnect
```

## Protocol (REST, `/api/v1`)

TLS at Traefik; per-device `Authorization: Bearer osn_…` for everything
under `/api/v1`, session cookie for the human dashboard.

```
POST /devices/enroll            {code} → {device_id, token}   (one-time code, TTL)
GET  /enroll                    public: enrollment landing page; with ?cb= and a
                                loopback host it 303s the code back to the client
GET/POST /devices               list / revoke (dashboard)
POST /folders                   {name, retention, encrypted}
POST /folders/{id}/members      {device_id, root_path, mode}
GET  /folders/{id}/members      members with device names (dashboard)
GET  /folders/{id}/manifest     file states (first sync / drift check)
POST /folders/{id}/ops          submit ops [{base_seq, kind, path, ...}]
GET  /folders/{id}/ops?since=N  tail of the log, ordered, idempotent
GET  /folders/{id}/history?path= version chain for a file (dashboard)
POST /folders/{id}/resolve      {path, restored_from} → resolve op
PUT  /chunks/{hash}             stream upload; 409 present = skip; verify sha256
GET  /chunks/{hash}             stream download; Range for resume
GET  /overwrites?since=N        review queue (superseded versions)
GET  /events                    activity feed
```

`POST /folders/{id}/members` infers the device from the bearer token, so a
client registers its own root; the `device_id` field is dashboard-only.
Adding these endpoints is backward-compatible, so `PROTOCOL_VERSION` is
unchanged.

Idempotency on `id`/`hash` everywhere: a client that crashed after
applying an op but before recording it just replays. A version commits
only when every chunk it lists exists (one transaction, else 409).
`PUT /chunks/{hash}` verifies the body against the hash before storing,
and a client verifies every downloaded chunk before its rename.

## Client behavior

- Self-write guard: every applied path is stamped with
  `(device_id, version_id)`; watcher events matching an applied version
  are swallowed, or the loop feeds itself.
- Atomic apply: `<name>.omni-tmp`, fsync, `os.Replace`, then chmod +
  utimes from the version. Symlinks are transferred as links, never
  followed out of the root.
- Offline: ops journal to `pending_ops`; reconnect replays idempotently
  with backoff. Chunk cache doubles as resume state.
- `.omniignore` per root (gitignore syntax) plus global excludes
  (`.DS_Store`, `*.tmp`, caches).

## Sync modes: live vs on-close

Not every file should sync the instant it changes. An Obsidian vault is
markdown that is *meant* to be captured mid-thought; a game save written
by a running game is a snapshot of a live process, and syncing it
mid-session produces a garbage version plus a history of garbage. So
every folder (or pattern within it) declares a mode:

```toml
[[folders]]
path = "~/Vaults/Notes"
mode = "live"                      # any change, debounced 500 ms
exclude = [".obsidian/workspace.json", ".obsidian/cache", ".trash/"]

[[folders]]
path = "~/Games/Saves"
mode = "on_close"                  # sync only when no process holds the file
hold_while_process = ["steam*", "retroarch", "dolphin-emu"]
quiet_timeout_secs = 30            # safety net when holders can't be detected
```

- **`live`** — debounce, hash, push, done. Obsidian note saved at 14:03
  is on the phone at 14:04. Excludes matter more than mode here:
  `.obsidian/workspace.json` and plugin `data.json` churn without
  carrying content, so they are excluded by default in vaults.
- **`on_close`** — a change marks the file `held`, and the agent syncs it
  only when **no process holds an fd or mmap on it**. "The program that
  had it open finished" is exactly the all-handles-released condition;
  we do not need to know the program's name.
- **`hold_while_process`** — folder-level hold while any matching process
  runs, which is the game case: a game that rewrites a whole save tree,
  or mmap's its data, would be corrupted by *our* apply as much as by our
  upload. While held, both directions defer: uploads wait, and remote
  ops queue instead of being applied over files the local process owns.

Detection (Linux, unprivileged, own uid): `/proc/*/fd/*` symlinks and
`/proc/*/maps` for the path, matching `/proc/<pid>/comm` + exe basename
against the globs. Our own pid is filtered out (we hold an fd while
hashing). On other platforms fall back to `lsof`/`fuser` if present, and
always to `quiet_timeout_secs` — a hold we cannot prove is released is
still bounded. Cost is capped: once the dirty set in a folder exceeds
~50 files, fd scanning stops and the quiet timer takes over, so a
`git status`-style churn storm degrades to "sync every 30 s", not to a
`/proc` walk per file.

`file_state` gains `held_reason` (`held_by_pid:1234`, `quiet_until`, 
`process:retroarch`) so `omni-sync status` explains itself ("held —
pid 4123 has save.dat open") and the dashboard's per-folder lag view
distinguishes "waiting" from "broken". `omni-sync flush` overrides both
holds for a folder or a single path.

Apply-side consequence worth stating: an apply is `tmp + os.Replace`,
which orphans the inode of a process that has the old file open — its
next write lands in a file nobody syncs. That is why `on_close` and
`hold_while_process` defer *inbound* files too, and why a held file is
never rename-replaced underneath its holder.

## Binary files

Binary is the *normal* case here, not a special case, and every layer is
byte-level by construction: FastCDC reads raw bytes with no charset
assumption, chunk ids are `sha256` of bytes, the relay stores and streams
bytes, and the client writes bytes back out. Photos, archives, videos,
SQLite DBs, `.parquet` — nothing in the transport or storage path knows
what a character is. The places that *do* need to care:

- **Paths are bytes, not strings.** A Linux filename is not guaranteed to
  be UTF-8. Store `folder_id, path` as a `BLOB` with a UTF-8 view for the
  API/dashboard; a non-UTF-8 path is synced losslessly, not coerced.
  Coercing (lossy `to_string_lossy`) is how a file named `résumé.pdf`
  on one box becomes a *different* file on another.
- **Never buffer a whole file.** Chunking is streaming (`BufReader` over
  the chunker), apply is streaming append into the tmp file, one fsync at
  rename. A 40 GB video must cost constant memory.
- **Small files** (≤ min chunk) are a single chunk, which is what makes
  `.gitkeep`, icons and thumbnails cheap.
- **Metadata is scope-limited on purpose**: size, mtime_ns, mode bits.
  Not carried in v1: xattrs/ACLs, macOS resource forks and Finder flags,
  extended attributes on Linux. Document them as lossy rather than
  silently drop them — `omni-sync doctor` warns when a file in a watched
  tree has meaningful xattrs.
- **Being-written files** (editors, video recorders, `*.crdownload`) look
  like churning garbage to the watcher: excluded by default (`.tmp`,
  `.part`, `.crdownload`, `~$*`, `Thumbs.db`), and a file whose mtime
  moves mid-hash is re-scanned rather than snapshotted half-written.

## Dashboard typing rules

The dashboard must never *render* a byte stream it does not understand:

- **Text** (decodes as UTF-8/UTF-16, under a size cap) → side-by-side or
  unified diff for the overwrite/history views.
- **Images** (`image/png`, `jpeg`, `gif`, `webp` — sniffed by magic bytes,
  *not* by extension, and **never** `image/svg+xml`, which is script)
  → inline preview plus size/mtime/hash/version metadata.
- **Everything else binary** → metadata card and an optional hex head,
  no diff. "Cannot diff binaries, here are both versions' hashes and
  sizes, restore either" is the honest UI.
- Any preview endpoint sets `X-Content-Type-Options: nosniff`, a tight
  `Content-Security-Policy`, and `Content-Disposition` chosen from the
  sniffed type, so a file's *contents* can never turn into script on the
  dashboard.

## Optional per-folder encryption (designed in, not built in v1)

Folder key = KDF(passphrase). Chunk key = HKDF(K, sha256(plaintext)) —
convergent, so identical plaintext still dedups to one ciphertext chunk,
and the server never sees a key. Clients get the passphrase at `add`
time; losing it means the relay copy is unrecoverable, which is the
point. Keeps `chunks` schema unchanged when it lands.

## Repo layout

```
omni-sync/
  Cargo.toml                 # workspace, edition 2024, rust-version 1.89
  crates/
    omni-core/               # ops, version chain, LWW/overwrite rule, diff — no I/O
    omni-chunk/              # FastCDC gear chunker + content hashing — no I/O
    omni-store/              # rusqlite, Store::blocking seam, forward-only migrations
    omni-netproto/           # wire types + versioning
    omni-transfer/           # reqwest client: ops, chunks, retry/backoff
    omni-server/             # axum app, sequencer, relay, templates/ + dashboard (bin omni-sync-server)
    omni-agent/              # watcher, push/pull loops, applier, policies
    omni-cli/               # client CLI + daemon (bin omni-sync, via explicit [[bin]])
  deploy/                    # Traefik dynamic file, systemd user units, Podman notes
  install.sh                 # cargo build --release --locked, install to ~/.local/bin
  docs/{architecture,protocol,api}.md
  plan.md
```

Toolchain note for this machine: `cargo +1.89.0 …` — the default
toolchain (1.88) is below the workspace floor and cannot build it.

## CLI

```
omni-sync-server init|serve|pair|doctor   # init writes config, prompts dashboard password
omni-sync-server folder create|list       # manage shared folders
omni-sync-server update [--check] [--force] [--to <tag>] [--download-only <dir>]
omni-sync enroll --link '<magic link>'    # browser handoff; or --server/--code
omni-sync add [<folder> [<path>]]         # wizard when args are omitted
omni-sync folders                        # folders, membership, applied_seq, device lag
omni-sync daemon                         # the watcher/agent (systemd per device)
omni-sync daemon install|uninstall|status|restart  # install it as a per-user service
omni-sync status [--folder]                     # held files shown with the reason
omni-sync history <folder> <path>        # version chain
omni-sync resolve <folder> <path> --to <version>   # rollback from the CLI too
omni-sync flush [folder] [path]          # override a hold, sync now
omni-sync doctor                         # server reachability, store, scan lag
omni-sync update [--check] [--force] [--to <tag>] [--restart-daemon|--no-restart]
```

Dashboard (HTMX partials from askama templates, no SPA, no Tauri): Login · Overview (devices
online, per-folder lag and held counts, storage, recent activity) · Devices (pairing
codes, fingerprints, roots, revoke) · Folders (members, policy, browse,
per-file **history chain + rollback**) · **Overwrites** queue (both
versions: diff for text, magic-byte preview for images, metadata card and
"restore older" action for everything else) · Activity feed.

## Milestones

- **M0 scaffold**: workspace, config (toml, layered), both schemas +
  migrations, both binaries' CLI roots, `serve` behind Traefik with
  pairing-code enrollment and an authenticated empty dashboard.
- **M1 core**: chunker (determinism + shift tests), version chain,
  LWW/overwrite rule, scan/apply — pure, unit-tested, no I/O.
- **M2 server**: devices/folders/versions/chunks, chunk endpoints,
  manifest, op commit (LWW + overwrite rows), resolve endpoint.
- **M3 client**: enroll, watch, debounce, chunk+upload, pull loop,
  atomic apply. Two daemons converge over loopback.
- **M4 dashboard**: pages above, history view, overwrite queue, rollback.
- **M5 hardening + deploy**: journal/replay, resume, retention/GC,
  quota, `fsck`/repair, systemd units, Traefik + LE on the VPS, install
  and release, verified **from outside** over the public URL (the
  space-elevator rule: green tests have hidden broken deploys).
- **M6 onboarding + updates** (built):
  - **Magic-link enrollment.** The dashboard shows a link
    `<server>/enroll?code=…`. `omni-sync enroll --link '<link>'` binds a
    loopback listener and opens a browser; the public `/enroll` page
    redirects the browser back to the listener (host validated to
    loopback, nonce-checked), which enrolls and writes the config. The
    `--server`/`--code` path and a prompt wizard both still work.
  - **Folder-add wizard.** `omni-sync add` with no arguments lists the
    server's folders (`Select`) and prompts for a local root with
    filesystem path completion; it then records membership on the
    server. Non-interactive flags are unchanged.
  - **Membership.** `members` (which existed unused) now has
    `POST/GET /api/v1/folders/{id}/members`; the dashboard shows each
    folder's devices, roots and lag. Still recorded, not enforced.
  - **Updates.** `omni-sync update` and `omni-sync-server update` query
    GitHub Releases, pick the asset for the compiled-in target, verify
    it against the release's `SHA256SUMS`, and install it atomically.
    Releases publish bare binaries alongside the archives. The server
    container runs `docker-entrypoint.sh`, which prefers a binary staged
    on the `/state` volume by `update --download-only /state/bin`.
  - **Dashboard redesign.** One stylesheet, two bundled OFL fonts
    (Archivo + IBM Plex Mono), a petrol accent, and a left rail that
    collapses to a top bar on phones. The overview leads with a status
    banner instead of metric cards, and each folder shows a **sync rail**
    — `members.acked_seq` drawn against the op-log head — so "behind" is
    literal rather than a count. The overwrite queue compares the active
    and superseded versions side by side (device, time, size).
- **Later**: direct client↔client transfer, per-folder convergent
  encryption, bandwidth caps, more OSes, signed releases.

## Acceptance

- Unit: the sequencer table (stale base → LWW + overwrite row; absent
  path → create), chunker determinism and shift resistance (1 kB insert
  at offset 0 moves one chunk), scan/apply round-trip.
- Integration on one host: two daemons, two roots → edit on A appears on
  B < 2 s; kill a daemon mid-transfer → resume re-requests nothing.
- Public deploy: dashboard login rejects anonymous; API rejects a bad
  token with 401; `curl https://sync.albruiz.dev/` → 303/200 from the
  outside, not just localhost.
- Overwrite flow: A and B edit the same file apart → LWW applies B,
  overwrite row appears, `resolve --to <A's version>` → both devices
  converge on A's content.
- Live vs on-close: an Obsidian-style vault syncs a save within ~1 s;
  a test process holding an fd on `save.dat` in an `on_close` folder
  keeps it unsynced with `held_by_pid` reported, and it syncs within
  2 s of the fd closing. A folder with `hold_while_process` matching a
  running fake "game" process uploads nothing and applies nothing until
  that process exits. `quiet_timeout_secs` bounds both when holder
  detection is unavailable.

## Daemon service installation (2026-10-14)

`omni-sync daemon install|uninstall|status` installs the daemon as a
per-user service so it starts automatically and survives crashes. The
platform definitions live in `crates/omni-cli/src/service.rs` as pure
string generators (unit-tested from any host), with only the install /
uninstall / status steps behind `#[cfg(target_os)]`:

- **Linux**: a systemd *user* unit (`~/.config/systemd/user/omni-sync.service`),
  `Restart=on-failure`, and a printed `loginctl enable-linger` hint. `--system`
  writes `/etc/systemd/system/omni-sync.service` with `User=` set instead.
- **macOS**: a LaunchAgent in `~/Library/LaunchAgents/` with `RunAtLoad` +
  `KeepAlive`, logging to `~/Library/Logs/omni-sync.log`.
- **Windows**: a per-user logon Task Scheduler task (not an SCM service).

Generated files use absolute paths and a `Managed by omni-sync` marker, so
re-installing is idempotent and only a foreign file needs `--force`. The
daemon now treats SIGTERM like Ctrl-C, so a service stop cleans up
`<stem>.daemon.ctl` and its engines. See `deploy/README.md`.

**Restart on update.** `omni-sync update` restarts the service so the new
binary takes effect: `--restart-daemon` forces it, `--no-restart` suppresses
it, and on a terminal with a service installed it asks (default yes). Only a
*running* service is restarted (`try-restart` semantics). The same code backs
`omni-sync daemon restart`. The decision is a pure, unit-tested function; a
failed restart is reported but never fails an update that already installed
the binary.
