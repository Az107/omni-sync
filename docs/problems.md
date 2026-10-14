# omni-sync — Problem register

Working list of bugs and product problems, collected before any code is
written. Each entry says **what is observed**, **what the code actually
does**, and **how it was confirmed** (reproduced against the real binaries,
or read from the source).

Status legend: `fixed` = resolved and verified; `confirmed` = reproduced or
plainly visible in code.

All items P1–P6 below (plus three additional engine/watcher root causes found
during implementation, P8–P10) are **fixed**.

---

## P1 — A newly added folder is not synced until the daemon is restarted

**Confirmed.**

`omni-sync add` writes the folder into `client.toml`, but a daemon that is
already running never re-reads that file. `daemon()` loads the config once,
spawns one `Engine` per entry, and then only waits for Ctrl-C
(`crates/omni-cli/src/main.rs:380`). There is no config watcher, no
`SIGHUP`, and no signal from `add` to the running process.

Consequence: on an always-on device (the normal deployment), the flow

```
omni-sync daemon          # running, watching folders A, B
omni-sync add <new> <dir> # user adds folder C
```

prints `watching folder <new> at <dir>` but folder C is **never** synced
until the daemon is stopped and restarted. This is exactly the reported
"new added folder is not synced at the beginning": if the folder is added
on the device that holds the files, that device never pushes them, so the
server stays empty and the other device has nothing to pull.

Reproduction (this repo, Linux 0.3.0):

1. Start `omni-sync daemon` on a client that watches one folder.
2. Create a second server folder; `omni-sync add <second> <dir>` with a
   file under `<dir>` while the daemon runs.
3. `GET /api/v1/folders/<second>/ops?since=0` → `{"entries":[]}`; the
   daemon log shows only the original folder being synced.

Note: the same daemon **does** sync a folder that was in the config at
startup — both the "empty A / non-empty B" and the reverse converge when
both daemons are (re)started after `add`. So the first-sync path itself is
sound; the trigger is the stale running daemon.

---

## P2 — `omni-sync add` accepts a folder id the server does not have, and reports success

**Confirmed.**

`add()` never checks that the folder exists. It saves the entry to
`client.toml`, then tries `register_member` as a best-effort call and only
prints a note on failure (`crates/omni-cli/src/main.rs:307`). Because the
`members` table has a foreign key to `folders`, registering a non-existent
folder returns **500 `storage error`** — but `add` still prints
`watching folder <id> at <dir>` and exits `0`.

Reproduction:

```
$ omni-sync add no-such-folder /tmp/x
note: the server did not record this membership (server refused the request (500): {"error":"storage error"})
watching folder no-such-folder at /tmp/x
$ echo $?
0
```

The broken entry stays in the config, and every later daemon run keeps
trying to sync it and failing. Wanted: creating a folder from the client
(so `add` can create-then-watch), and a hard failure when the folder is
absent.

---

## P3 — There is no "sync now"

**Confirmed.** Both directions are daemon-driven only.

`omni-sync flush` (the intended override) and `history` / `resolve` are
stubs that print `not implemented yet — it lands in M5`
(`crates/omni-cli/src/main.rs:163`). An `Engine::sync_once` (one push +
pull) already exists in `crates/omni-agent/src/engine.rs:120` but is not
reachable from the CLI.

Consequence: after adding a folder, copying a large tree, or changing
network conditions, the user has no way to force a cycle without starting
the full daemon and waiting for an event or the 30 s safety scan. There is
also no way to ask a *running* daemon to sync now.

---

## P4 — Live mode (and `on_close`, and holds) is not wired at all

**Confirmed in code; macOS symptom reported.**

The `mode` chosen by `add` ("live" / "on_close") is stored in
`client.toml` and the wizard asks about it, but the engine never reads it.
`EngineConfig` carries only `folder_id` and `root`
(`crates/omni-agent/src/engine.rs:39`), so:

- `on_close` behaves exactly like `live` (no holding);
- `SyncMode` and `HoldRule` (`crates/omni-agent/src/policy.rs`) are dead
  code;
- `omni_agent::held()` always returns `None`
  (`crates/omni-agent/src/hold.rs:25`).

The reported symptom is "live mode is not working, at least on macOS".
Since the mode is ignored, on this build "live" is what every folder does
already — so the macOS symptom is about the **watcher/daemon actually
running and reacting**, not about the policy. The likely macOS-specific
causes (to confirm on the device, see the plan):

- the macOS client is not being kept running as a service (there is no
  launchd unit; systemd units are listed in the plan but macOS is not);
- FSEvents latency/debounce, or the daemon not being restarted after
  `add` (P1);
- a release/binary mismatch (which version the Mac client reports).

Whatever the macOS trigger, the deeper bug stands on its own: the sync
mode is a promise the code does not keep.

---

## P5 — Overwrites are presented as problems, but they are the rollback/safety feature

**Confirmed.**

Last-write-wins means two devices that edit the same path while apart
**always** create an `overwrites` row; that is by design and is how history
is kept. But the dashboard treats every open row as an unresolved alarm:

- `status_summary` ranks any overwrite first and labels the banner `bad`:
  `"N to review"` / _"Two devices changed the same file while apart. Keep
  one; the other stays stored."_ (`crates/omni-server/src/app.rs:1102`).
- The overview counts `sync::overwrites(conn, true)` as the top-line
  problem.
- Rows are only cleared by an explicit *Restore this version*
  (`mark_overwrite_resolved`), so a routine offline edit leaves a
  permanent red state even after both devices have converged on identical
  content.

Wanted: present overwrites as **recoverable history / rollback points**,
not as a fault queue. A normal, converged overwrite should read as "a
conflict was resolved automatically; both versions are kept", with a
tranquil visual treatment, and the red banner reserved for genuinely
broken sync (a device that cannot catch up).

---

## P6 — The magic link is less convenient than copying a code; roll it back

**Confirmed (product decision).**

The dashboard's "Add a device" mints a code and renders a **pairing link**
as the primary action (`templates/devices.html`), and the enrollment page
pushes `omni-sync enroll --link '…'` (`templates/enroll.html`). The magic
link opens a loopback listener and a browser handoff
(`crates/omni-cli/src/link.rs`).

The user finds this worse than reading a short code from the dashboard and
typing/pasting it. Wanted: revert the default onboarding to **show the
pairing code** in the dashboard (and keep `enroll --server --code` as the
documented path). The magic link can be kept as an optional extra or
removed; it must not be the prominent flow.

---

## P7 — Adjacent gaps found while investigating (lower priority, listed for completeness)

- **`add` conflates "watch" and "sync".** After `add`, nothing syncs until
  a daemon is started (and, per P1, restarted if one was already running).
  There is no first-sync-on-add.
- **`GET /api/v1/overwrites?since=`** ignores `since` and returns all open
  rows (`crates/omni-server/src/app.rs:857`). Harmless today, misleading
  as an API.
- **Membership is recorded, not enforced.** Any enrolled device may read
  or write any folder. Known and documented; not a regression.
- **Move ops are refused** (`StoreError::Unsupported`); a rename is a
  delete + put at the client. Known and documented.
- **No chunk GC**, and `verify` (full-content re-check) is not built.
  Known and documented.
- **Server-side lag can look wrong after a restart.** `members.acked_seq`
  is durable but a device that has been offline can show as "behind" long
  after it is actually caught up if it has not pulled since. Tied to P5's
  banner logic; verify before changing.

---

## P8 — `watcher::watch` forwarded read-only access events, starving the pull stream

**Fixed.**

`notify::recommended_watcher` on Linux (`inotify`) subscribes to `IN_OPEN`
and `IN_CLOSE_NOWRITE`. Every `Engine::push` calls `scan::scan`, which calls
`fs::read_dir` and `fs::read`. Forwarding those read-only events queued a
new watcher event *while `push` was still running* (after the debounce
drain). Because `events.recv()` sat above `wake.recv()` in a `biased`
`tokio::select!`, the engine spun in a 250 ms self-scan loop and starved
the SSE pull stream (`wake.recv()`).

Fix: `watcher::is_relevant` filters out read-only access events and ignored
`.omni-tmp` paths, and `wake.recv()` sits above `events.recv()` in
`Engine::run_with_control`.

---

## P9 — Unconditional pull-before-push wiped out offline edits on initialized folders

**Fixed.**

Pulling before pushing is right on a folder's *first* sync (`applied_seq ==
0`), so pre-existing identical files adopt the server's `version_id` instead
of raising false overwrites. However, on an already-initialized folder
(`applied_seq > 0`), pulling before pushing overwrote local offline edits on
disk via `apply::write_file` and updated `file_state` before `push` ever
scanned them.

Fix: `Engine::sync_step` pulls before pushing when `applied_seq == 0` (while
preserving pre-existing local files whose content differs from the server)
and pushes before pulling when `applied_seq > 0`.

---

## P10 — Cross-folder `ops.id` gaps left watermarks behind and re-downloaded own pushes

**Fixed.**

`ops.id` is `AUTOINCREMENT` across all folders on the server, so ops in
Folder A create sequence gaps in Folder B. `advance_watermark` stops at any
gap in case a foreign op in the same folder landed inside it. Without a
follow-up pull after `submit_ops`, `applied_seq` and `members.acked_seq`
stayed stuck behind the device's own push, and the next pull re-downloaded
and rewrote the device's own files.

Fix: `push_inner` reconciles with `pull_inner` after submitting ops, and
`pull_inner` skips `fetch_chunks`/`write_file` when
`versions.get(&path) == Some(&entry.version_id)` and the file exists on
disk.
