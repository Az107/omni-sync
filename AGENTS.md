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

**M0 is done and verified end to end.** Workspace, config layering,
SQLite schemas for both sides, `init`/`serve`/`pair`/`doctor`,
pairing-code enrollment, and an Argon2-gated dashboard. The smoke path
was run for real: `init` → `serve` → `pair` → client `enroll` → device
in `doctor` → dashboard login → a reused code is refused.

**M1 is partly done.** The content-defined chunker (`omni-chunk`) and the
last-write-wins sequencer (`omni-core`) exist with tests. Still to do for
M1: file hashing, the scan/diff/apply path.

Folders, the op/chunk endpoints, the watcher loop and the dashboard
pages are M2–M4 and are **not built yet** — the client subcommands print
the milestone they land in.

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
- **`omni-core` and `omni-chunk` have no dependencies at all.** That is
  the point: the rules and the chunker are tested without a runtime, a
  database or a socket. Keep I/O out of them.
- **Paths are bytes.** `omni_core::PathBytes` is `Vec<u8>`. The wire
  base64-encodes them (`omni-netproto`), SQLite stores them as `BLOB`.
  Never round-trip a path through `String`/`to_string_lossy`.
- **`omni-store` has three access shapes**: `blocking`/`blocking_mut`
  (async, `spawn_blocking`) and `with`/`with_mut` (sync, for CLI paths).
  `*_mut` exists only for transactions — currently just pairing-code
  consumption.
- **The server binds loopback.** TLS and the public hostname are
  Traefik's job; do not add a TLS listener here.
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

## Known gaps (not bugs)

- The client daemon is a stub: `add`, `daemon`, `status`, `folders`,
  `doctor` print the milestone they land in. Only `enroll` is wired.
- `omni_agent::held()` always returns `None`. The `/proc` holder
  detection (live vs on-close) is M3.
- No ops or chunk endpoints yet; the dashboard has one page (overview)
  and no HTMX interactions.
- `omni-core`'s `Op`/`Version` and `omni-netproto`'s DTOs are
  deliberately separate types; the conversions between them land with
  the endpoints in M2.
