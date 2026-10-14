# omni-sync — Fix plan

Plan for the problems in [`problems.md`](problems.md), now implemented and
verified. See [`../AGENTS.md`](../AGENTS.md) ("Engine & Watcher Invariants")
for the guardrails that must be preserved on future changes.

Guiding rules (from `AGENTS.md`): the server stays the only sequencer;
LWW never deletes history; migrations are forward-only; paths stay bytes;
`omni-core`/`omni-chunk` stay I/O-free.

---

## Sequencing

Ordering matters because later fixes build on earlier ones.

1. **P4 groundwork + P1** — make the daemon own a live folder set and be
   able to sync a folder on demand. Both are prerequisites for a good
   `add`.
2. **P2** — make `add` validate/create folders and fail loudly.
3. **P3** — expose "sync now".
4. **P6** — dashboard onboarding rollback (independent, can land anytime).
5. **P5** — overwrite presentation (independent, dashboard only).

P1/P2/P3 are one coherent change to the client lifecycle; do them
together. P6 and P5 are separate, low-risk, dashboard-only.

---

## P1 — New folder not synced until restart

**Goal:** `add` takes effect immediately on a running daemon.

**Approach (preferred): daemon watches its own config.**

- `daemon()` watches the config file with the existing `notify` watcher
  (reuse `omni_agent::watcher` or a small dedicated watch) and, on change,
  diffs the folder set:
  - new entry → spawn an `Engine` and keep its handle;
  - removed/edited entry → stop that engine;
  - unchanged → leave it running.
- Keep the engine tasks in a `HashMap<folder_id, JoinHandle>` and abort
  tasks that are removed. The current code already `tokio::spawn`s each
  engine, so this is an extension of `daemon()`, not a new subsystem.
- Debounce config writes (the file is rewritten by `save_config`, which is
  a truncate+write, so a raw watcher can see a partial file). Read-retry
  or write-then-rename in `save_config` would make the watch robust; a
  write-then-rename is the cleaner fix and also protects a crash mid-write.

**Alternative / complement:** a fast local control channel (unix socket or
a tiny loopback endpoint in the daemon) that `add`/`sync-now` poke. This
also serves P3 ("sync now" against a running daemon) and is more explicit
than inferring intent from a file. If we build this, put it behind the
same module for both P1 and P3.

**Files:** `crates/omni-cli/src/main.rs` (`daemon`, `save_config`),
possibly a new `crates/omni-cli/src/daemon.rs` holding the folder-set
manager; `crates/omni-agent` only if the engine needs a stop/abort signal
(it does not today — `abort()` is enough).

**Tests:**
- Unit: config diff → set of folders to start/stop, given old and new
  configs (pure function, no I/O).
- Integration/e2e script: start daemon with one folder, `add` a second,
  assert the second folder's ops appear on the server within ~3 s
  (the exact repro from P1).

**Risk:** watchers see the temp/partial write; mitigated by
write-then-rename. Watch the config's *directory* too, since a rename
replaces the inode.

---

## P4 — Wire up sync mode and holds

**Goal:** the mode the user picks means something; `on_close` actually
holds, and the macOS "live" path is trustworthy.

**Step 0 — diagnose macOS first (no code).** On the Mac client collect:
`omni-sync doctor`, the client version, whether a daemon is running as a
service, and the daemon log around a file edit. Decide whether the
symptom is P1 (stale config), a missing service, or a real watcher bug.
Do not "fix" live mode until we know which. *(Ask the user to run this.)*

**Step 1 — thread the mode through.** `EngineConfig` gains `mode`
(`SyncMode`) and any `HoldRule` from the config; `daemon()` parses the
`WatchEntry.mode` string into `SyncMode` (reject unknown values loudly).
Update `crates/omni-cli/src/main.rs` and `crates/omni-agent/src/engine.rs`.

**Step 2 — `live`.** Already the behaviour; the change is mostly naming
and making the config honest. Keep debounce.

**Step 3 — `on_close` + holds.** Implement `held()`:
- Linux: scan `/proc/*/fd` and `/proc/*/maps` for the path (filter our own
  pid), and `/proc/<pid>/comm`/exe basename against `HoldRule` globs.
- macOS/other: `lsof`/`fuser` if present.
- Always fall back to `quiet_timeout_secs`.
Then in the engine: a changed file in `on_close` is marked held and not
uploaded until released; inbound ops for held paths defer too.

**Step 4 — surface it.** `file_state` gains `held_reason`; `status`
prints it. This is plan.md's on-close design; the schema is an additive
forward-only migration in `crates/omni-store/migrations/`.

**Files:** `crates/omni-agent/src/{engine,policy,hold,scan}.rs`,
`crates/omni-cli/src/main.rs`, new agent migration,
`crates/omni-store/src/agent.rs`.

**Tests:** unit-test `glob_match`/hold decisions (already partly there);
a Linux test spawns a process holding an fd on a file in an `on_close`
folder and asserts it syncs only after the fd closes; `quiet_timeout`
bounds a hold.

**Defer:** full macOS holder detection if `lsof` is unreliable; the
quiet timeout is the safe floor.

---

## P2 — `add` on a non-existent folder

**Goal:** either create the folder, or fail before mutating local state.

**Approach:**
- Add a client command to create a folder: `omni-sync folder create <name>`
  (mirrors `omni-sync-server folder create`; `Client::create_folder`
  already exists in `crates/omni-transfer/src/lib.rs:84`).
- In `add`, resolve the folder *first*:
  - `list_folders` and verify the id exists;
  - if the argument is not an id but a name and no folder matches, offer to
    create it (interactive) or fail.
- Treat `register_member` failure as fatal: remove the just-added config
  entry and exit non-zero. Never leave a `watching folder` line for a
  folder that was rejected.
- Non-interactive: fail with a clear message pointing at
  `omni-sync folder create`.

**Files:** `crates/omni-cli/src/main.rs` (new subcommand + `add`),
`crates/omni-cli/src/wizard.rs` (folder picker/create prompt),
`crates/omni-transfer` (already has `create_folder`).

**Tests:** unit tests for the folder-resolution pure logic; e2e: `add`
with a bogus id exits non-zero and leaves no entry; `add --new <name>`
creates and watches.

**Migration note:** fixing `add` should also make later daemons tolerate
(and clean up) already-broken entries left by the old behaviour — dedupe
and skip ids the server rejects, with a warning.

---

## P3 — "sync now"

**Goal:** a way to force a cycle, both one-shot and against a running
daemon.

**Approach:**
- Implement `omni-sync flush [folder] [path]` (currently a stub) as
  "run one push+pull now" using the existing `Engine::sync_once`. With no
  daemon running, this is a one-shot sync; with a daemon running, use the
  control channel from P1 to ask it to sync (and to override holds, per
  the `flush` design in plan.md).
- Optionally add `omni-sync sync` as an alias / clearer name; decide the
  final surface with the user.
- Have `add` call a one-shot sync at the end (after P2 validation) so a
  freshly added folder starts moving immediately even without a daemon —
  this is the "sync at the beginning" the user expects.

**Files:** `crates/omni-cli/src/main.rs`, `crates/omni-agent/src/engine.rs`
(`sync_once` exists), plus the P1 control channel if we build it.

**Tests:** e2e: `add` then `flush` on a non-running daemon moves files
without starting `daemon`; `flush` against a running daemon completes a
cycle promptly.

**Risk:** don't make `flush` a back door that runs a second engine
concurrently with the daemon on the same folder and the same agent DB.
Route it through the running daemon when one exists.

---

## P6 — Roll back the magic link to a code-first flow

**Goal:** dashboard "Add a device" shows a code to copy; `enroll
--server --code` is the documented path.

**Approach:**
- `devices.html`: make the **code** the primary element (large,
  monospace, copy button optional), with the `omni-sync enroll --server
  <url> --code <code>` command. Demote or remove the link.
- `enroll.html`: lead with the code; keep the link flow only if we decide
  to retain it as an alternative.
- Keep `crates/omni-cli/src/link.rs` only if we keep the optional path;
  no change needed to `enroll --server/--code`, which already works.
- `omni-sync-server pair` still mints codes; unchanged.

**Files:** `crates/omni-server/templates/{devices,enroll}.html`,
`crates/omni-server/src/app.rs` only if the mint action changes (it
already redirects to `/devices?code=…`).

**Tests:** dashboard render tests/eyeball; the existing `pair` e2e still
passes.

**Decision needed:** keep `--link` as an undocumented extra, or delete it
and `link.rs`. Recommend keeping the code path and leaving `--link` in
place but not advertised, to avoid a breaking change for whoever used it.

---

## P5 — Overwrites as history, not alarms

**Goal:** a converged conflict reads as "kept both, can roll back",
not a fault.

**Approach:**
- Rework `status_summary` precedence in
  `crates/omni-server/src/app.rs`: "a device that cannot catch up" and a
  failed sync outrank overwrites. Overwrites become an informational
  state, not `bad`.
- Reframe the copy: "N versions to review" → something like "N conflicts
  resolved automatically; both versions kept (rollback available)".
- Reconsider whether an open row should keep the dashboard permanently
  non-green. Options: treat "converged" overwrites as history (auto-mark
  seen once every member has acked past the winning op), or move them out
  of the banner entirely into a **History / Versions** page.
- Prefer the history page: overwrites and the per-file version chain
  belong together. This also gives the rollback UI a permanent home
  (`resolve` already exists server-side and in the dashboard as *Restore*).
- Keep *Restore this version*; it is correct.

**Files:** `crates/omni-server/src/app.rs` (`status_summary`,
`dashboard`, `overwrites_page`, `folder_views`), templates
`dashboard.html`, `overwrites.html`, maybe rename to `history.html`,
`crates/omni-store/src/sync.rs` if we add an auto-"seen" rule.

**Tests:** unit-test the new `status_summary` precedence (it is already
pure and tested); make sure a converged overwrite no longer forces the
`bad` banner.

**Risk:** don't hide genuine divergence. Distinguish "both devices acked
past the conflict and hold the same content" (benign) from "the losing
device still has different content" (needs attention). The signal for
benign is `members.acked_seq` past the winning op *and* the active
version being the winner.

---

## Explicitly out of scope for this pass

- Chunk GC, `verify`, retention, membership enforcement, move ops,
  direct client↔client transfer, TLS at the server, signing releases.
  These stay "known gaps" until the six above are done.
- Do not, while fixing P1/P3, weaken the "server is the only sequencer"
  rule or the LWW-never-deletes-history rule.
