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

**M0–M4 and M6 are done, plus the client lifecycle, sync modes (`live` /
`on_close`), and CLI history/resolve/flush commands.** A server and two
client daemons were run against each other and verified for: create
(including binary files, byte-identical on both sides), bidirectional
edits within ~2 s, deletes, adding a new folder while daemons are already
running (live config reload + control handoff), `on_close` holding an open
file with `held_by_pid:<pid>` until the descriptor closes, offline edits on
both devices producing one overwrite row with last-write-wins convergence,
and rolling back a file via both the dashboard and `omni-sync resolve`.
91 unit tests, clippy `-D warnings` clean.

What exists:

- Pure core (`omni-core`, `omni-chunk`), wire contract (`omni-netproto`),
  and both SQLite schemas (`omni-store`: server + agent).
- Server (`omni-server`) with `/api/v1` (enrollment, folders, members,
  manifest, ops + SSE stream, per-path history, resolve, overwrites,
  chunks) and the server-rendered dashboard (overview, devices + pairing
  codes, folders + sync rails, overwrites with *Restore this version* and
  *Keep active version*).
- Client engine (`omni-agent`) with `notify` watcher, metadata-triage scan,
  atomic apply, `live` and `on_close` policies, `/proc` holder and process
  glob detection (`held_files` + `file_state.held_reason`), and SSE-woken
  pull.
- Client CLI (`omni-cli`) with `enroll`, `add` (validates/creates folders,
  registers membership, triggers initial sync), `folder create|list`,
  `daemon` (live config reload + loopback control socket), `flush` (alias
  `sync`), `status` (including held files), `history`, `resolve`, `doctor`,
  and `update`.

**Change detection is event-driven at both ends.** Push runs on `notify`
mutation events (inotify/FSEvents/`ReadDirectoryChangesW`) with a 30 s
safety scan; pull is woken by the server's op-log SSE stream
(`GET /api/v1/folders/{id}/ops/stream`), with a 60 s fallback poll for a
server that has no stream or a connection that died silently. Scanning
reuses the last-synced chunk list for any file whose size/mtime/mode are
unchanged, so an idle folder does no disk reads.

## Versioning

**The client and the server version and release independently.**

- **`omni-cli` and `omni-server` carry their own `version`** in their crate
  manifests and are the source of truth for `--version`, `/healthz` and the
  updater. Release them on their own tag trains: **`client-v<version>`** and
  **`server-v<version>`**.
- **The shared libraries** (`omni-core`, `-chunk`, `-netproto`, `-store`,
  `-transfer`, `-agent`, `-update`) use `[workspace.package] version`. Bump it
  (plus the `[workspace.dependencies]` path entries) only when a library
  changes, and release **both** binaries then, since both see the new library.
- **`omni-update` never reads `env!("CARGO_PKG_VERSION")` itself** — that would
  resolve to the library's version. Each binary passes its own
  (`current_version(env!("CARGO_PKG_VERSION"))`), and discovery scans the
  release list for the newest tag matching the binary's prefix rather than
  GitHub's repo-wide `/releases/latest`, which cannot distinguish the two.
- **Tag == the binary's version.** The updater compares the parsed tag against
  the running binary's version, so a tag that drifts from the manifest makes a
  self-update think it is already current and silently do nothing.

Compatibility is **not** the semver. It is `omni_netproto::PROTOCOL_VERSION`
(`1` today), which `enroll` checks; it is bumped only when the wire shape
changes incompatibly. A client and a server on different versions but the same
`PROTOCOL_VERSION` interoperate — so an old server with a new client is fine as
long as no DTO/endpoint changed. Rule of thumb for either component:

- **patch**: no wire/contract change and no new required peer capability.
- **minor**: a user-facing feature, or anything protocol-affecting.
- **`1.0`**: protocol freeze; `PROTOCOL_VERSION` then tracks the major.

**Transition note.** A client built before component tags (≤ 0.4.1) reads
GitHub's repo-wide `latest` and a bare `v*` tag, so `release.yml` still accepts
a bare `v*` tag as a client build. Until every device is on a component-aware
client (≥ 0.4.2), cut client releases as `v<version>` once so they can
auto-update; only the server may move straight to `server-v*`.

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
cargo run -p omni-cli -- folder create Docs
cargo run -p omni-cli -- add --create Docs ~/Docs   # or `add` with no args for the wizard
cargo run -p omni-cli -- daemon
cargo run -p omni-cli -- daemon install             # systemd user unit / launchd agent / logon task
cargo run -p omni-cli -- daemon status
cargo run -p omni-cli -- daemon restart             # pick up a freshly installed binary
cargo run -p omni-cli -- daemon uninstall
cargo run -p omni-cli -- flush                      # sync now (alias: `sync`)
cargo run -p omni-cli -- status
cargo run -p omni-cli -- history Docs notes/todo.md
cargo run -p omni-cli -- resolve Docs notes/todo.md --to 1
cargo run -p omni-cli -- update --check             # is a newer release out?
cargo run -p omni-cli -- update --restart-daemon    # update, then restart the service
```

CI (`.github/workflows/ci.yml`) runs fmt, clippy, `cargo test --workspace`
and release-builds both binaries on every push and PR; `release.yml`
attaches a component to a GitHub release when a **`client-v*`** or
**`server-v*`** tag is pushed (or via **Run workflow**), and only builds that
component's targets. Both pin Rust 1.89.0 to match `rust-toolchain.toml`.

Release matrix — the client ships for six targets, the server for two:

| package | targets |
|---|---|
| `omni-sync` (client) | `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` (native arm64 runner), `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc` |
| `omni-sync-server` | `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` (native arm64 runner) |

Both server targets matter: the space-elevator host is arm64, so the
in-container `update` needs an `aarch64` asset to exist, not just the
container build.

`ubuntu-24.04-arm` is a GitHub-hosted arm64 Linux runner (free for public
repos), so `aarch64-unknown-linux-gnu` is native rather than cross-built.
The release also publishes bare binaries (not just the archives), which is
what `omni-sync update` downloads; both are covered by `SHA256SUMS`.

`Swatinem/rust-cache` is keyed per target *and* package (`key: <target>-
<package>`); the default job-based key is shared by every matrix entry, so
without this the entries overwrite each other's `target/` dir. Only native
targets are smoke-tested — `x86_64-apple-darwin` and
`aarch64-pc-windows-msvc` are cross-built and cannot run on the runner.
`AWS_LC_SYS_PREBUILT_NASM=1` is set because the Windows image has no NASM
and `aws-lc-sys` x64 builds need it (ignored off Windows x86-64).

### Smoke test

Self-contained; uses `/tmp/opencode` so it never touches real per-user dirs.

```sh
mkdir -p /tmp/opencode/omni-e2e && cat > /tmp/opencode/omni-e2e/server.toml <<'TOML'
addr = "127.0.0.1:8799"
data_dir = "/tmp/opencode/omni-e2e/data"
state_dir = "/tmp/opencode/omni-e2e/state"
dashboard_password_hash = ""
TOML

OMNI_SYNC_PASSWORD=smoke ./target/debug/omni-sync-server --config /tmp/opencode/omni-e2e/server.toml init
./target/debug/omni-sync-server --config /tmp/opencode/omni-e2e/server.toml serve &
curl -s http://127.0.0.1:8799/healthz                     # {"ok":true,...}
CODE=$(./target/debug/omni-sync-server --config /tmp/opencode/omni-e2e/server.toml pair | head -1)
./target/debug/omni-sync --config /tmp/opencode/omni-e2e/client.toml \
  enroll --server http://127.0.0.1:8799 --code "$CODE" --name smoke
./target/debug/omni-sync-server --config /tmp/opencode/omni-e2e/server.toml doctor   # devices: 1
```

What to check: `/` anonymously is `303` to `/login`; a login `POST`
sets the cookie and the overview renders; enrolling with the same code
twice returns `400 invalid, expired or already-used pairing code`.

## Engine & Watcher Invariants (Traps That Break Sync)

Read this section before touching `omni-agent` or `omni-cli::daemon`. Every
rule below comes from a real bug that broke two-way sync or lost offline
edits.

1. **`watcher::watch` must filter out read-only `notify` events
   (`EventKind::Access(Open | Read | Close(Read))`).**
   - *Trap*: On Linux (`inotify`), `notify::recommended_watcher` subscribes
     to `IN_OPEN` and `IN_CLOSE_NOWRITE` by default. Every time
     `Engine::push_inner` runs `scan::scan`, `fs::read_dir` and `fs::read`
     open directories and files under the watched root. If `watcher::watch`
     forwards read-only access events, `scan::scan` queues a new event
     *while `push_inner` is running* (after the debounce drain). In a
     `biased` `tokio::select!`, `events.recv()` is then immediately ready on
     the next loop iteration, locking the engine into an infinite 250 ms
     self-scan loop and **starving `wake.recv()` (the pull stream) forever**.
   - *Rule*: `watcher::is_relevant` only forwards `Create`, `Modify`,
     `Remove`, `Any`, and `Access(Close(Write))`, and drops events whose
     paths are all ignored temp files (`.omni-tmp`). In
     `Engine::run_with_control`, `wake.recv()` also sits above
     `events.recv()` in the `biased` `select!`.

2. **First sync (`applied_seq == 0`) pulls before pushing; initialized sync
   (`applied_seq > 0`) pushes before pulling (`Engine::sync_step`).**
   - *Trap A (pushing first when `applied_seq == 0`)*: When a device joins a
     folder that already has files on the server and already has a copy of
     those files on disk, pushing before pulling sends every file with
     `base_version_id = None` (since `file_state` is empty), turning the
     entire directory into false overwrite rows.
   - *Trap B (pulling first when `applied_seq > 0`)*: When an already-synced
     device (`applied_seq > 0`) edits a file while offline and reconnects,
     pulling unconditionally before pushing downloads the remote version and
     calls `apply::write_file` + `agent::set_file` over the local file
     *before* `push_inner` ever scans it — silently wiping out the user's
     offline edit without creating an overwrite row.
   - *Trap C (pre-existing local files on first sync)*: During first sync
     (`!versions.contains_key(&path)`), if `pull_inner` sees `disk_path`
     already exists locally, it hashes the local file first:
     - if `local_meta.equivalent(&remote_meta)` (same chunks + mode), it
       records `entry.version_id` in `file_state` without rewriting;
     - if the local content differs, it leaves the local file intact on disk
       and records a synthetic baseline (`mtime_ns = 0`, `version_id = 0`)
       in `file_state` so the immediately following `push_inner` uploads the
       local file and records an overwrite on the server (keeping both
       versions).

3. **`ops.id` is one global sequence across ALL folders — `push_inner` must
   reconcile `pull_inner` after pushing, and `pull_inner` must skip
   re-downloading its own `version_id`.**
   - *Trap*: `ops.id` is `INTEGER PRIMARY KEY AUTOINCREMENT` on a single
     table shared by all folders. Pushing to Folder A advances `ops.id`,
     creating a sequence gap in Folder B. `advance_watermark` (rightly)
     advances `applied_seq` only across contiguous sequence numbers starting
     at `previous`, because a gap *might* contain a foreign op in the same
     folder. Without an immediate follow-up `pull_inner` after `submit_ops`:
     - `applied_seq` on the client and `members.acked_seq` on the server
       (which is updated by `GET /ops`) remain stuck behind the device's own
       push, so the dashboard sync rail shows the pushing device as "behind";
     - and when `pull_inner` later tails `ops?since=<old>`, the server
       returns the device's own just-pushed ops. If `pull_inner` does not
       check `versions.get(&path) == Some(&entry.version_id) &&
       disk_path.exists()`, it re-downloads the chunks it just uploaded and
       rewrites the file on disk.
   - *Rule*: Keep `advance_watermark` strict (contiguous run from
     `previous`), call `pull_inner` at the end of `push_inner` whenever ops
     were submitted, and in `pull_inner` short-circuit (advance
     `applied_seq` without `fetch_chunks`/`write_file`) when
     `versions.get(&path) == Some(&entry.version_id)` and the file exists on
     disk. (Every `Put` or `Resolve` on the server mints a strictly new
     `versions.id`, so equality means this client already has that exact
     version.)

4. **Never run a second `Engine` concurrently with `omni-sync daemon` on the
   same `agent.db`.**
   - *Trap*: Running `omni-sync flush` or `omni-sync add`'s initial sync by
     opening a second `Engine` in the CLI process while `daemon` is already
     watching the same config causes two processes to race on `file_state`,
     `applied_seq`, and `.omni-tmp` renames.
   - *Rule*: `daemon::run` binds an authenticated loopback listener
     (`127.0.0.1:0`, credentials written to `<stem>.daemon.ctl` with mode
     `0600`) and hands each folder's `Engine::run_with_control` a channel of
     `FlushRequest`s. `add`, `flush` (`sync`), and `resolve` call
     `daemon::try_send_command` first so the running `Engine` executes the
     sync inside its own `select!` loop; only when no daemon is running does
     the CLI run a one-shot `Engine` in-process.

5. **`spawn_op_waker` must select on `wake.closed()`.**
   - *Trap*: When `daemon` removes or reconfigures a folder, it aborts the
     `Engine` task, dropping the `mpsc::Receiver<()>`. If `spawn_op_waker`
     only ignores `wake.try_send(())` errors, the background SSE task never
     notices the receiver is gone and reconnects to `/ops/stream` forever.
   - *Rule*: Every await point inside `spawn_op_waker` (`stream_ops`,
     `next_head`, and the backoff sleep) must be wrapped in
     `tokio::select! { _ = wake.closed() => return, ... }`.

6. **Config saves (`write_private`) must be atomic (write-temp-then-rename),
   and config watching must watch the parent directory.**
   - *Trap*: Truncating `client.toml` in place lets the daemon's watcher read
     a half-written or empty TOML file. Conversely, atomic rename replaces
     the file's inode, which causes a single-file `inotify` watch to stop
     firing after the first save.
   - *Rule*: `write_private` writes `<file>.tmp.<pid>` (`0600`), calls
     `sync_all()`, and renames over `<file>`. `daemon::watch_config_dir`
     watches `config_path.parent()` non-recursively (filtering to mutation
     events on `config_path.file_name()`) plus a 1 s content check fallback.

7. **Tracing logs must go to `stderr` (`.with_writer(std::io::stderr)`).**
   - *Trap*: `tracing_subscriber::fmt()` defaults to `stdout`. Subcommands
     like `omni-sync-server pair` and `omni-sync folder create` print bare
     codes/IDs to `stdout` for shell capture (`CODE=$(... pair | head -1)`).
     Any log line on `stdout` (such as `applied migration`) corrupts scripts.

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
  `*_mut` exists only for transactions.
- **The server binds loopback.** TLS and the public hostname are
  Traefik's job; do not add a TLS listener here.
- **The client DB and daemon control file are keyed by the config file's
  stem** (`a.toml` → `a.agent.db`, `a.daemon.ctl`), not just its directory.
  Two clients in one directory (as in tests) never share state.
- **Apply does not restore mtime.** Content (the chunk list) and mode are
  what matter; restoring mtime needs `utimensat`/libc and is deferred. A
  `FileMeta` equality therefore includes mtime, but sync decisions use
  `FileMeta::equivalent` (chunks + mode), and `pull_inner` records the
  on-disk `local_mtime_ns` after writing so the next scan's metadata fast
  path matches.
- **Sync modes (`live` vs `on_close`) and holds.** `EngineConfig` carries
  `SyncMode` and `HoldRule` (`process_globs`, `quiet_timeout_secs`). In
  `on_close` mode (or when `hold_while_process` matches a running process),
  both outbound uploads and inbound overwrites of held files defer, and
  `agent.db` records the reason in `held_files` and `file_state.held_reason`
  (`held_by_pid:<pid>`, `process:<name>`, `quiet_until`). While holds exist,
  `Engine::run_with_control` enables a 500 ms retry tick so releasing an fd
  or exiting a process syncs within ~0.5 s; when no holds exist, the retry
  tick is disabled.
- **askama's `render()` is a trait method.** `use askama::Template as _;`
  must be in scope where you call it, not only where the struct is
  defined.
- **The dashboard is one stylesheet plus small templates.** `static/app.css`
  holds the design tokens (`:root` + `prefers-color-scheme`), two bundled
  fonts (Archivo for language, IBM Plex Mono for machine data; SIL OFL, in
  `static/fonts/`), and every component. Pages extend `templates/base.html`
  and share `_rail.html` (navigation) and `_folder.html` (a folder with its
  sync rail). Monospace is reserved for ids, paths, hashes and commands.
  `static_asset` serves `font/woff2` with a one-hour cache.
- **The sync rail is the dashboard's signature.** It draws `members.acked_seq`
  against the folder's op-log head, so "behind" is literal. The server records
  a member's watermark on every pull (`GET /folders/{id}/ops` →
  `members::set_acked`); without that the rail would always read behind. The
  overview leads with a status banner (`status_summary`, unit-tested): device
  lag (`behind > 0`) raises `warn`, whereas converged overwrites (`behind ==
  0`) keep the banner `ok` ("All clear") and note how many superseded
  versions are kept for rollback.
- **axum 0.8 catch-all routes are `{*path}`**, not `/*path`.
- **`omni-update` derives the target triple from `build.rs`** (which
  re-emits the build script's `TARGET` as `OMNI_TARGET`). Asset selection
  therefore matches `.github/workflows/release.yml` exactly; there is no
  runtime OS/arch table to drift. Updates verify against the release's
  `SHA256SUMS` only — no signature yet.
- **The server container runs `docker-entrypoint.sh`**, which prefers a
  binary on the `/state` volume (`update --download-only /state/bin`)
  over the one baked into the image. That is what makes a self-update
  survive a redeploy; delete the staged file to fall back to the image.
- **CLI prompts only when stdin is a TTY.** `omni-sync enroll`/`add`
  become wizards on a terminal and demand flags otherwise, so scripts and
  CI are unaffected.
- **`omni-sync daemon install` writes a per-user service, not a system
  one.** Linux gets a systemd *user* unit (with a printed
  `loginctl enable-linger` hint; `--system` opts into `/etc/systemd/system`
  with `User=` set so `0600` control files stay user-owned), macOS a
  LaunchAgent, Windows a logon Task Scheduler task (never an SCM service —
  session 0 has no user profile). Generated files are absolute-pathed and
  carry a `Managed by omni-sync` marker, so re-install is idempotent and
  only a foreign file needs `--force`; installing a `target/{debug,release}`
  binary is refused. `daemon` also handles SIGTERM like Ctrl-C so a service
  stop removes `<stem>.daemon.ctl` cleanly. `update` restarts the service so
  the new binary runs (`--restart-daemon`/`--no-restart`, or a prompt on a
  TTY), and `daemon restart` does the same on demand — but only a *running*
  service is restarted, and a failed restart never fails the update. See
  `crates/omni-cli/src/service.rs` and `deploy/README.md`.
- **Config never holds a secret.** The dashboard password is an Argon2
  PHC string; device tokens are stored only as SHA-256
  (`auth::hash_token`). Files under the state dir are opened `0600`.

## Invariants that must not be relaxed

- **The server is the only sequencer.** `omni_core::Sequencer::decide`
  is the single ordering rule; no device clock may order versions.
- **Last-write-wins never deletes history.** A superseded version is kept
  so the dashboard or `omni-sync resolve` can roll back; retention is the
  only thing that removes rows.
- **Migrations are forward-only.** Append to `migrations::MIGRATIONS` or
  `migrations::AGENT_MIGRATIONS`; never edit an applied script.
- **A chunk's hash is `sha256` of its bytes** and is verified on upload
  and after download. Nothing is stored under a hash it was not proven
  to match.
- **The dashboard must never render bytes it does not understand.** When
  previews land: text → diff, images → inline (never SVG), all else →
  metadata only, and every preview sets `X-Content-Type-Options: nosniff`
  plus a tight CSP.

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
state and the chunk store on every redeploy. **space-elevator v0.1.0 mounts
fresh, empty volumes on every `se apps update`** — it strips the compose's
`volumes:` and never populates its own `app_storage` mapping, so the server
comes up with an empty database; the "backup" is metadata-only. Prefer a
binary-only update inside the running container (`omni-sync-server update
--download-only /state/bin` then `se apps restart`, which reuses the
container's volumes); if `se apps update` is unavoidable, carry the old
volumes across by hand. See the deploy README.

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

- **Move ops are refused** (`StoreError::Unsupported`); a rename is a
  delete + put at the client. The op kind exists in the schema.
- **Per-folder membership is not enforced.** Any enrolled device may
  read or write any folder. Membership is *recorded* (`members`, via
  `POST /api/v1/folders/{id}/members`) and shown on the dashboard, but
  the server does not check it on read or write.
- **The metadata fast path trusts mtime.** A file edited in place with
  its timestamp preserved is not detected until its metadata changes.
  That is the deliberate triage rule; a full-content `omni verify` is the
  escape hatch and is not built yet.
- **No chunk refcount GC.** `chunks.refs` is maintained on write; nothing
  reclaims unreferenced chunks yet.
- The dashboard is plain form posts (no HTMX yet): overview, devices
  with pairing codes, folders, and overwrites with *restore* / *keep active*.
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
other within ~2 s; an edit propagates both ways; a delete propagates; adding
a second folder via `omni-sync add --create` while both daemons are running
syncs immediately without a restart; and an offline edit on both sides
leaves one entry in `GET /api/v1/overwrites` with both devices converged on
the same content. `RUST_LOG=info` makes the daemon print `pushed changes`
per cycle.
