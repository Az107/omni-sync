# omni-sync

[![CI](https://github.com/Az107/omni-sync/actions/workflows/ci.yml/badge.svg)](https://github.com/Az107/omni-sync/actions/workflows/ci.yml)

Automatic file sync between devices: an always-on **server** with a
dashboard (devices, folders, version history, overwrites) and a watching
**client** daemon per device.

The full design lives in [`plan.md`](plan.md). This repository is the M0
scaffold: a compiling Rust workspace with the pure sync core, the
chunkers, both database schemas, and the server/client binary shells.

## Layout

| crate | responsibility |
|---|---|
| `omni-core` | ops, versions, the last-write-wins sequencer rule — no I/O |
| `omni-chunk` | content-defined chunking (gear-hash FastCDC) — no I/O |
| `omni-netproto` | the wire contract (`/api/v1`) |
| `omni-store` | SQLite, forward-only migrations, the `blocking` seam |
| `omni-transfer` | HTTP client for the server API |
| `omni-agent` | the client daemon: watcher, holds, apply (M3) |
| `omni-cli` | the `omni-sync` client binary |
| `omni-server` | the server binary: `/api/v1` + HTMX dashboard |

## Build

```sh
cargo +1.89.0 build
cargo +1.89.0 test --workspace
```

The workspace floor is Rust 1.89 / edition 2024. The `default` toolchain
on this host is older, so use `+1.89.0` (or rely on `rust-toolchain.toml`).

## Releases

Tagging `v*` (or running the **Release** workflow manually with a tag)
builds and attaches prebuilt binaries, each with a `SHA256SUMS`:

| package | targets |
|---|---|
| `omni-sync` (client) | linux `x86_64`, macOS `arm64` + `x86_64`, Windows `arm64` + `x86_64` |
| `omni-sync-server` | linux `x86_64` (also ships as a container) |

macOS and Windows builds are `tar.gz` / `zip` respectively. The macOS
`x86_64` and Windows `arm64` builds are cross-compiled, so they are not
executed in CI; the others are smoke-tested before packaging.
