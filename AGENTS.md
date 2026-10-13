# AGENTS.md — start here

Entry point for any session working on **omni-sync**. Read
[`plan.md`](plan.md) first: it is the design, and this file is the map of
what exists and the traps already hit.

## What this project is

Automatic file sync with two parts:

- **`omni-sync-server`** — an always-on coordinator with a dashboard.
  Registers devices, defines shared folders, sequences every change,
  keeps every version, relays chunks, and is where overwrites get
  resolved.
- **`omni-sync`** — a watching client daemon per device.

One Rust workspace, several crates, two binaries.

## Status

**M0–M4 are done; the MVP syncs and is manageable.** A server and two
client daemons were run against each other and the following held: a
create (including a binary file, byte-identical on both sides), an edit
propagating both ways within ~2 s, a delete propagating, and an offline
edit on both devices producing exactly one overwrite row with
last-write-wins converging both devices on the same content. The
dashboard was then used to *restore older* on that overwrite, and both
devices converged on the superseded content with the queue cleared. 47
tests, clippy clean.

What exists: the whole pure core (`omni-core`, `omni-chunk`), the wire
contract, both SQLite schemas, the server with its `/api/v1` surface,
chunk store and dashboard (overview, devices + pairing codes, folders,
overwrites + resolve), and the client engine (`omni-agent::Engine`) that
scans, pushes ops+chunks and applies the pulled log.

**M5 (hardening) is not built.** `history`, `resolve` and `flush` client
subcommands still print their milestone; there is no chunk GC and no
per-folder membership enforcement. See "Known gaps".

**Change detection is watcher-driven.** Push runs on `notify` events
(inotify/FSEvents/`ReadDirectoryChangesW`) with a 30 s safety scan; pull
runs on a 2 s timer because remote changes are not filesystem events.
Scanning reuses the last-synced chunk list for any file whose
size/mtime/mode are unchanged, so an idle folder does no disk reads.
Measured: ~1.6 s end-to-end propagation, ~5% of one core when idle.

## Commands

```sh
cargo build                       # toolchain pinned by rust-toolchain.toml
cargo test --workspace            # hermetic: no network, no sockets
cargo clippy --all-targets -- -D warnings
cargo fmt --all

# server
cargo run -p omni-server -- init
cargo run -p omni-server -- serve
cargo run -p omni-server -- pair          # mint a one-time code
cargo run -p omni-server -- doctor

# client
cargo run -p omni-cli -- enroll --server http://127.0.0.1:8787 --code ABCD-EFGHJ
```

CI (`.github/workflows/ci.yml`) runs fmt, clippy, `cargo test --workspace`
and release-builds both binaries on every push and PR; `release.yml`
attaches `omni-sync` and `omni-sync-server` tarballs to a GitHub release
when a `v*` tag is pushed. Both pin Rust 1.89.0 to match
`rust-toolchain.toml`.

### M0 smoke test

Self-contained; uses `/tmp` so it never touches real per-user dirs.

```sh
mkdir -p /tmp/omni-e2e && cat > /tmp/omni-e2e/server.toml <<'TOML'
addr = "127.0.0.1:8799"
data_dir = "/tmp/omni-e2e/data"
state_dir = "/tmp/omni-e2e/state"
dashboard_password_hash = ""
TOML

OMNI_SYNC_PASSWORD=smoke ./target/debug/omni-sync-server --config /tmp/omni-e2e/server.toml init
./target/debug/omni-sync-server --config /tmp/omni-e2e/server.toml serve &
curl -s http://127.0.0.1:8799/healthz                     # {"ok":true,...}
CODE=$(./target/debug/omni-sync-server --config /tmp/omni-e2e/server.toml pair | head -1)
./target/debug/omni-sync --config /tmp/omni-e2e/client.toml \
  enroll --server http://127.0.0.1:8799 --code "$CODE" --name smoke
./target/debug/omni-sync-server --config /tmp/omni-e2e/server.toml doctor   # devices: 1
```

What to check: `/` anonymously is `303` to `/login`; a login `POST`
sets the cookie and the overview renders; enrolling with the same code
twice returns `400 invalid, expired or already-used pairing code`.

## Non-obvious structure

- **Binary names come from explicit `[[bin]]`.** `omni-cli` produces
  `omni-sync`; `omni-server` produces `omni-sync-server`. Relying on the
  package name will surprise you.
- **`omni-core` and `omni-chunk` carry no I/O.** `omni-core` depends on
  serde (its value types derive it) and `omni-chunk` on `sha2`/`hex`;
  neither touches a file, a socket or a database. That is what keeps the
  rules and the chunker testable in isolation.
- **Paths are bytes.** `omni_core::PathBytes` is `Vec<u8>`. The wire
  base64-encodes them (`omni-netproto`), SQLite stores them as `BLOB`.
  Never round-trip a path through `String`/`to_string_lossy`.
- **`omni-store` has three access shapes**: `blocking`/`blocking_mut`
  (async, `spawn_blocking`) and `with`/`with_mut` (sync, for CLI paths).
  `*_mut` exists only for transactions — currently just pairing-code
  consumption.
- **The server binds loopback.** TLS and the public hostname are
  Traefik's job; do not add a TLS listener here.
- **The client DB is keyed by the config file's name** (`a.toml` →
  `a.agent.db`), not just its directory. Two clients sharing one DB was a
  real bug: they clobbered each other's `file_state` and every file
  became a delete/overwrite storm.
- **`omni-agent::Engine` pushes before it pulls, and records its own
  result.** A push sets `file_state` (with the returned `version_id`) and
  advances `applied_seq` past its own ops, so the following pull never
  re-applies what this device just sent. `base_version_id` comes from the
  per-path version map, and is sent as `None` (not `0`) for a path that
  has never synced.
- **Push and pull run on separate cadences, on purpose.** Push is
  `notify`-driven (inotify/FSEvents/`ReadDirectoryChangesW`) with a 30 s
  safety scan, because it must walk and hash the tree. Pull is a 2 s timer
  of the op log, because a remote change is not a local filesystem event;
  SSE/WebSocket push is the eventual replacement. Scanning reuses the
  last-synced chunk list for files whose size/mtime/mode are unchanged, so
  an idle folder does no disk reads. Inotify can drop events (queue
  overflow, new directories, NFS), which is what the safety scan is for.
- **Apply does not restore mtime.** Content (the chunk list) and mode are
  what matter; restoring mtime needs `utimensat`/libc and is deferred. A
  `FileMeta` equality therefore includes mtime, but sync decisions use
  `FileMeta::equivalent` (chunks + mode).
- **askama's `render()` is a trait method.** `use askama::Template as _;`
  must be in scope where you call it, not only where the struct is
  defined.
- **axum 0.8 catch-all routes are `{*path}`**, not `/*path`.
- **Config never holds a secret.** The dashboard password is an Argon2
  PHC string; device tokens are stored only as SHA-256
  (`auth::hash_token`). Files under the state dir are opened `0600`.

## Invariants that must not be relaxed

- **The server is the only sequencer.** `omni_core::Sequencer::decide`
  is the single ordering rule; no device clock may order versions.
- **Last-write-wins never deletes history.** A superseded version is kept
  so the dashboard can roll back; retention is the only thing that
  removes rows.
- **Migrations are forward-only.** Append to `migrations::MIGRATIONS`;
  never edit an applied script.
- **A chunk's hash is `sha256` of its bytes** and is verified on upload
  and after download. Nothing is stored under a hash it was not proven
  to match.
- **The dashboard must never render bytes it does not understand.** When
  previews land (M4): text → diff, images → inline (never SVG), all else
  → metadata only, and every preview sets `X-Content-Type-Options:
  nosniff` plus a tight CSP.

## Toolchain gotchas

- **`rust-toolchain.toml` pins 1.89.0**, so inside the repo plain
  `cargo`/`rustc` already resolve to it even though the machine's default
  is older. Use `cargo +1.89.0` when the manifest is not the one in
  scope (CI, scripts run from elsewhere).
- **The chunker's rolling hash must shift (`h << 1`), never
  `rotate_left`.** A rotate keeps every byte's contribution forever, so
  an insertion near the start shifts every later boundary and nothing
  resynchronises. A unit test asserts resync; it failed on exactly this
  and the fix was the shift.
- **`thiserror` must be a dependency wherever you derive
  `thiserror::Error`.** A missing manifest entry makes the derive not
  run, and then every `?` in that file fails with a wall of
  "needs to implement `From`" errors that looks like a logic problem.
- **Clocks below 1.89 / edition < 2024 cannot build the workspace**, and
  the code uses let-chains (`if let ... && let ...`) which require
  edition 2024.

## Deploying

The server ships as a container with a space-elevator compose stack:
`Dockerfile`, `compose.yml`, and `deploy/space-elevator/README.md`. The
compose file is what makes the platform use managed named volumes
(`omnidata` → `/data`, `omnistate` → `/state`); a plain Dockerfile drop
gets a synthetic compose with no persistence, which would lose the SQLite
state and the chunk store on every redeploy.

**Deployed and verified from the outside** at
`https://elevator.albruiz.dev/app/omni-sync/` — health, login, pairing,
static assets, and a real client enrolling and syncing a file over the
public URL.

Two non-obvious constraints that cost time, both in the deploy README:

- **The container listens on :80 and runs as root inside the container.**
  SE's rootful Traefik reaches a host-published web app on that port; a
  non-root user cannot bind it. A non-root build on port 8787 came up
  `502` through Traefik while the container logged `listening`.
- **Drive SE through the `se` wrapper**, which needs the `service-admin`
  group. The group is applied at login; `sg service-admin -c 'se ...'`
  refreshes it in a shell that predates the group.

Env the container understands: `OMNI_SYNC_ADDR`,
`OMNI_SYNC_DATA_DIR`, `OMNI_SYNC_STATE_DIR`, and `OMNI_SYNC_PASSWORD`
(hashed in memory when no stored hash exists, so no config file is baked).
The client appends API paths *under* its base URL, so a sub-path
deployment works unchanged.

## Known gaps (not bugs)

- `history`, `resolve` and `flush` still print "not implemented yet". The
  server-side `resolve` endpoint exists; only the client subcommands are
  stubs.
- **Move ops are refused** (`StoreError::Unsupported`); a rename is a
  delete + put at the client. The op kind exists in the schema.
- **Per-folder membership is not enforced.** Any enrolled device may read
  or write any folder. `members` exists but is unused.
- The dashboard covers overview, devices (with pairing codes), folders
  and overwrites (with *restore older*). It is plain form posts, no HTMX
  yet, and there is no per-file history view.
- **The metadata fast path trusts mtime.** A file edited in place with
  its timestamp preserved is not detected until its metadata changes.
  That is the deliberate triage rule; a full-content `omni verify` is the
  escape hatch and is not built yet.
- **No chunk refcount GC.** `chunks.refs` is maintained on write; nothing
  reclaims unreferenced chunks yet.
- **`omni_agent::held()` always returns `None`.** On-close/live modes are
  modelled (`SyncMode`, `HoldRule`) but holder detection is not built.
- The dashboard is plain form posts (no HTMX yet): overview, devices
  with pairing codes, folders, and overwrites with *restore older*.
- `omni-core`'s `Op`/`Version` and `omni-netproto`'s DTOs are deliberately
  separate types, bridged by `omni-netproto::convert`.

### Verification recipe (two clients)

```sh
# server + a folder + two enrollments, then two daemons on two dirs
FOLDER=$(./target/debug/omni-sync-server --config $R/server.toml folder create Docs)
# ... enroll a.toml and b.toml, `add $FOLDER` each dir ...
./target/debug/omni-sync --config $R/a.toml daemon &
./target/debug/omni-sync --config $R/b.toml daemon &
```

What to check: a file written in one dir appears byte-identical in the
other within ~2 s; an edit propagates both ways; a delete propagates; and
an offline edit on both sides leaves exactly one entry in
`GET /api/v1/overwrites` with both devices converged on the same content.
`RUST_LOG=info` makes the daemon print `pushed changes` per cycle.
