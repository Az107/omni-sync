# omni-sync — Flows (the macro picture)

A map of how the parts fit together and how work moves through them. This
is deliberately coarse: names, directions and responsibilities, not
function-by-function detail. For exact behaviour read the crates; for
what is broken read [`problems.md`](problems.md) and
[`fix-plan.md`](fix-plan.md).

---

## 1. The system at a glance

Two binaries, one protocol, one workspace. A server that is always on and
a daemon per device.

```
   device A (laptop)            server (always on)              device B (desktop)
  ┌────────────────┐         ┌──────────────────────┐         ┌────────────────┐
  │ omni-sync      │  HTTPS  │ omni-sync-server     │  HTTPS  │ omni-sync      │
  │  • watcher     │ ──────▶ │  • /api/v1 (devices, │ ◀────── │  • watcher     │
  │  • chunker     │  ops +  │    folders, ops,     │  ops +  │  • chunker     │
  │  • agent.db    │  chunks │    chunks, stream)   │  chunks │  • agent.db    │
  │  • chunk cache │ ◀────── │  • sequencer + LWW   │ ──────▶ │  • chunk cache │
  └────────────────┘   SSE   │  • versions + history│   SSE   └────────────────┘
                             │  • chunk store (dedup)│
                             │  • dashboard (HTML)   │
                             └──────────────────────┘
                                ▲ human operator
                                │ browser: login, devices, folders, overwrites
```

Rules that shape every flow:

- **The server is the only sequencer.** Op ids are one global sequence;
  device clocks never order anything.
- **All traffic goes through the server** (relay + backup); there is no
  direct device-to-device path yet.
- **Last-write-wins, but history is kept.** A superseded version is never
  deleted by LWW; only retention removes rows.

Crate roles:

| crate | job | I/O? |
|---|---|---|
| `omni-core` | ops, version chain, LWW rule, manifest diff | no |
| `omni-chunk` | FastCDC chunker + sha256 | no |
| `omni-store` | SQLite (server + agent), forward-only migrations | DB only |
| `omni-netproto` | wire DTOs + `PROTOCOL_VERSION` | no |
| `omni-transfer` | reqwest client for `/api/v1` | HTTP |
| `omni-server` | axum app, sequencer, chunk store, templates | yes (bin `omni-sync-server`) |
| `omni-agent` | watcher, scan, apply, sync engine | yes |
| `omni-cli` | client CLI + daemon (bin `omni-sync`) | yes |
| `omni-update` | release discovery + verify + self-install | HTTP/fs |

---

## 2. Enrollment — one device gets a token

A device starts with no trust. The server hands out one-time pairing codes;
a code is exchanged exactly once for a durable device token.

```
operator                server                     new device
   │  "Add a device"        │                            │
   ├───────────────────────▶│ mint code (TTL 10 min)     │
   │◀─────── code ──────────┤                            │
   │                                                     │
   │  omni-sync enroll --server <url> --code <code>      │
   ├────────────────────────────────────────────────────▶│
   │                                        POST /devices/enroll {code,name}
   │                                        server: consume code (one-shot),
   │                                        create device, issue token
   │◀──────── { device_id, token } ──────────────────────┤
   │                                        write client.toml (0600):
   │                                        server, device_id, token, folders[]
```

- A code lives in `pairing_codes` and is consumed in the same transaction
  that inserts the device, so two devices cannot race one code.
- The token is stored only as a sha256 hash on the server; the plaintext
  exists only in the client config.
- The dashboard leads with the pairing code and the `omni-sync enroll
  --server <url> --code <code>` command (`--link` remains supported for
  compatibility).
- Enrolling the dashboard doesn't need the code; the operator password
  (Argon2 PHC in config) gates the HTML pages with a session cookie.

---

## 3. Folder lifecycle — create, assign, first sync

A folder is a named, server-side sync scope. Devices declare that they
hold it at a local root (a *member*), then sync.

```
 server                          device B (has files)         device A (empty)
 ───────                         ────────────────────         ────────────────
 create folder "Docs"  ──┐
   (dashboard, server CLI,│
    or `omni-sync folder  │
    create` / `add --new`)│
                         ▼
 omni-sync add <id|name> <dir> ─▶ validates/creates folder on server
                                  registers member on server
                                  writes client.toml atomically (temp+rename)
                                  ┌───────────────────────────────────────┐
                                  │ pokes running daemon via .daemon.ctl  │
                                  │ (or runs sync_once if no daemon runs) │
                                  └───────────────────────────────────────┘
                                  push → server stores ops + versions + chunks
                                                      │
                                                      ▼
                                         SSE wake ─▶ A pulls the ops
                                         A fetches chunks, writes files
```

Key points:

- "Member" = `(folder, device, root, mode, acked_seq)`. Recorded today,
  not enforced.
- `omni-sync add` verifies the folder on the server and registers
  membership *before* mutating `client.toml`.
- A first sync (`applied_seq == 0`) runs **pull-before-push** so a device
  that joins later does not report the whole server tree as its own
  overwrite; once initialized (`applied_seq > 0`), the engine runs
  **push-before-pull** so offline local edits are sequenced with their
  `base_version_id` before incoming remote ops are applied.
- A running `daemon` watches `client.toml`'s parent directory and listens
  on `<stem>.daemon.ctl` (`0600`), so adding or reconfiguring a folder
  takes effect immediately without restarting the daemon.

---

## 4. Steady-state sync — two event-driven directions

The daemon keeps one engine per folder. Push and pull are independent.

### Push (local → server)

```
notify event  ──debounce 250 ms──▶  scan tree
safety scan every 30 s ──────────▶  (catches lost events / offline edits)
                                     │
                    for each changed file:
                    reuse last-synced chunks if size+mtime+mode unchanged
                                     │ else read + chunk + sha256
                                     ▼
                    upload chunks the server lacks  (PUT /chunks/{hash})
                                     ▼
                    POST /folders/{id}/ops  [{base_seq, base_version_id, …}]
                                     ▼
                    server applies (sequencer), returns {seq, version_id}
                                     ▼
                    client records file_state + advances applied_seq
```

### Pull (server → local)

```
server op log grows  ──publish──▶  SSE  GET /folders/{id}/ops/stream
                                     (carries only the head seq)
                                     ▼
                    wake the engine
                                     ▼
                    GET /folders/{id}/ops?since=<applied_seq>
                                     ▼
                    for each op: fetch + verify chunks, apply atomically
                    (write temp, fsync, rename, chmod), record file_state
                                     ▼
                    advance applied_seq
```

- The SSE stream is only a **wake-up**; ops always come from `ops?since=`,
  so there is one diff implementation.
- A 60 s fallback poll covers a server without the stream or a dead
  connection; the 30 s safety scan covers lost push events.
- The **watermark** (`applied_seq`) only advances across the contiguous
  run of ops this device actually applied — op ids are global, so a
  foreign op can sit inside our batch.
- Scan reuses the last chunk list when metadata is unchanged, so an idle
  folder does no disk reads. The trade-off is that an in-place edit with a
  preserved timestamp is missed until metadata changes (a `verify` command
  is the intended escape hatch and is not built).

### Idempotency and safety

- Ops carry path bytes (base64 on the wire, BLOB in SQLite) — never a
  lossy string.
- A version commits only when every chunk it names exists.
- Chunks are verified against their hash on upload **and** on download.
- Apply is `temp + rename`; content and mode are restored, mtime is not
  (so sync decisions use chunks + mode, not mtime).

---

## 5. Conflicts — LWW now, rollback later

Concurrent edits do not block. The server orders them and keeps both.

```
device A edits note.txt (base = v5)      device B edits note.txt (base = v5)
        │                                        │
        └──────────────┬─────────────────────────┘
                       ▼
            server applies in receipt order
            winner: whoever the server applied last
            loser:  a new `overwrites` row is written
                    (losing_version_id, winning_version_id)
                       │
                       ▼
            next pull converges every device on the winner
                       │
                       ▼
            dashboard can "Restore this version":
              the loser's content is re-emitted as a NEW version
              (restored_from = loser), so it propagates by ordinary sync
              and the overwrite row is marked resolved
```

- Nothing is lost: the loser is a stored `version`.
- Every version ever written lives in `versions`; `files.active_version_id`
  points at the current one.
- Once all devices have caught up (`behind == 0`), the dashboard overview
  banner stays `ok` ("All clear") and notes how many superseded versions are
  kept for rollback; the `/overwrites` page lets the operator either
  *Restore this version* or *Keep active version*.

---

## 6. Deletes (and the move gap)

- A delete is a normal op: the server writes a version with no chunks and
  marks the path `deleted_at`; clients apply it by removing the file.
- **Move is not implemented.** A rename at the client is seen as
  delete(old) + put(new); the server refuses an explicit `move` op
  (`StoreError::Unsupported`). The op kind exists in the schema for later.

---

## 7. Sync modes — `live` vs `on_close`

Each folder declares a mode (`live` or `on_close`) and optional `HoldRule`
(`hold_while_process`, `quiet_timeout_secs`):

- **`live`** — debounce (250 ms), hash, push, and apply remote changes
  immediately.
- **`on_close`** — checks `/proc/*/fd` and `/proc/*/maps` on Linux (falling
  back to `lsof`/`ps` and `quiet_timeout_secs` elsewhere). When a file is
  open (`held_by_pid:<pid>`), a matching process is running
  (`process:<name>`), or the quiet period is active (`quiet_until`), the
  engine records the hold in `agent.db` (`held_files` +
  `file_state.held_reason`), defers both uploading and overwriting the held
  file, and retries every 500 ms until released. `omni-sync flush` overrides
  holds on demand.

---

## 8. Dashboard

Server-rendered HTML (askama) over the same axum app as `/api/v1`, one
stylesheet and two bundled fonts. Session cookie for the operator.

| page | what it shows | acts as |
|---|---|---|
| Login | operator password | sets session cookie |
| Overview | one status sentence + folders with sync rails | landing/health |
| Devices | devices, presence; "Add a device" mints a code | enrollment |
| Folders | folders, their members and roots | folder management |
| Overwrites | pairs of versions (active vs superseded), *Restore* | history/rollback |

- The **sync rail** draws `members.acked_seq` against the folder's op-log
  head; the server records a member's watermark on every pull.
- Preview typing rule: text → diff, images → inline (never SVG), anything
  else → metadata only, with `nosniff` + a tight CSP.
- The dashboard never renders bytes it does not understand.

---

## 9. Updates

Both binaries can self-update from GitHub Releases. The target triple is
compiled in by `omni-update`'s build script, so asset selection matches
the release workflow exactly.

```
omni-sync update [--check|--force|--to <tag>]
  └─ query GitHub Releases → pick asset for this target
     → download → verify against the release's SHA256SUMS
     → install atomically (replace the running binary)

omni-sync-server update [--download-only /state/bin]
  └─ same, but can stage the binary on a volume instead of
     replacing the running one
```

The server container runs `docker-entrypoint.sh`, which prefers a binary
staged on `/state/bin` over the one baked into the image — that is what
lets a self-update survive a redeploy. Updates verify hashes only; there
is no signature yet.

---

## 10. Deploy (space-elevator)

```
Dockerfile + compose.yml  ──▶  Podman stack: se-omni-sync-web
   • listens on :80 as root (SE's Traefik reaches a host-published app there)
   • data → named volume /data (SQLite + chunks)
   • state → named volume /state (config + staged binary)
   • env: OMNI_SYNC_ADDR, OMNI_SYNC_DATA_DIR, OMNI_SYNC_STATE_DIR,
          OMNI_SYNC_PASSWORD
Traefik + Let's Encrypt terminate TLS; the server itself binds loopback.
```

Operational warning: `se apps update` remounts fresh, empty volumes and
would lose the database; prefer a binary-only in-container update and
`se apps restart`, or carry volumes across by hand.

---

## 11. Data model at a glance

**Server** (`server.db`): `devices`, `pairing_codes`, `folders`,
`members`, `ops` (append-only log), `versions` (every version ever),
`files` (active version per path), `overwrites` (review queue),
`chunks` (hash → size/refs), `events`.

**Client** (`<config-stem>.agent.db`): `file_state` (last-synced manifest
+ server version per path), `sync_state` (`applied_seq` per folder),
plus the on-disk chunk cache under the data dir.

Three views of a folder drive everything: **current** (on disk),
**last-synced** (`file_state`), **remote** (server op log/manifest).
`current vs last` = what to push; the op log = what to pull.

---

## 12. Trust boundaries

- `/api/v1/*` requires a per-device bearer token (stored hashed,
  revocable). Enrollment is the only public API route besides `/healthz`.
- Dashboard pages require a session cookie from the operator password.
- `/enroll` is public by necessity: the code *is* the secret, single-use
  and TTL'd; a loopback callback is validated to defeat open redirects.
- Config and state files are `0600`; config never holds a plaintext
  secret.
- TLS terminates at Traefik; the server binds loopback in the container.

---

## 13. Known gaps in the flows above

From `plan.md` / `AGENTS.md`, still true after these flows:

- membership recorded, **not enforced** (any device may touch any folder);
- move refused; no chunk GC; no `verify`;
- dashboard is plain form posts (no HTMX).
