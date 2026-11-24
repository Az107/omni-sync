# Hardening release verification — 2026-11-24

## Published

Release commit: `2f1f2d7e112b3c4bfcd8ab30850e13c7eacf43d4`.

| Component | Release | Assets |
|---|---|---|
| Client | [client-v0.4.2](https://github.com/Az107/omni-sync/releases/tag/client-v0.4.2) | six targets, bare binaries + archives + SHA256SUMS (13) |
| Server | [server-v0.4.1](https://github.com/Az107/omni-sync/releases/tag/server-v0.4.1) | two Linux targets, bare binaries + archives + SHA256SUMS (5) |

Protocol remains `1`. Every public asset was downloaded and verified against
the complete checksum set; every archive's executable matches its bare binary.
Neither release replaces the repository-wide legacy `latest` release.

## Validation

- [CI](https://github.com/Az107/omni-sync/actions/runs/45404893053): native
  Linux, macOS and Windows tests, formatting, strict clippy, both release
  builds, container build and isolated socket smoke all passed.
- 185 Rust tests pass on Linux, plus four release-contract tests.
- Native release build jobs test the workspace and assert binary versions;
  macOS x64 and Windows ARM64 are cross-built, not executed on those runners.
- [Client release](https://github.com/Az107/omni-sync/actions/runs/45406203445)
  succeeded after retrying a GitHub macOS runner-capacity cancellation. The
  cancelled job had never acquired a runner; no tag was moved.
- [Server release](https://github.com/Az107/omni-sync/actions/runs/45406203625)
  passed both Linux architectures and draft/downloaded-asset verification.
- Downloaded native ARM64 release binaries passed binary create/edit/delete,
  bidirectional sync, rollback, offline conflict convergence, control handoff,
  concurrent-daemon refusal, live folder reload, on-close hold/release,
  multi-page bootstrap, historical conflict preservation and idle-control
  shutdown tests.
- An isolated component-aware client built from the previous committed 0.4.1
  source discovered and installed published 0.4.2. Installed bytes equal the
  release checksum; a subsequent check reports up to date. This does not make
  the legacy `v0.4.1` release's updater component-aware: those installations
  still need one manual upgrade.
- The released server's updater discovered its own train, downloaded and
  verified a forced reinstall into an isolated staging directory, and ran it.
- The released server served a private copy of the production backup: all
  three original folders, 280 active files, 440 active chunks downloaded and
  hash-verified, and 60 overwrite rows were preserved.

## Production acceptance

`https://sync.albruiz.dev/healthz` reports `{"ok":true,"version":"0.4.1"}`.
PID 1 and direct published-port health also confirm 0.4.1.

The initial generic Python HTTP checker encountered Cloudflare's user-agent
filter and safely rolled back. Retrying with a descriptive validation user
agent passed public acceptance; this was not an application startup failure.

The final pre-upgrade private backup is:

`/home/services/.local/state/omni-sync/backups/pre-server-v0.4.1-smo8foei/`

It contains a consistent SQLite online backup, all 452 recorded chunks
(158,812,690 bytes, each hash/length verified), a metadata manifest and the
old 0.4.0 executable. Earlier rehearsal/checkpoint backups are also retained.
No chunk GC ran, so taking the database snapshot before copying immutable
referenced chunks produced a consistent backup without downtime.

The upgrade atomically replaced the staged executable and restarted the
existing container. **No `se apps update` or volume remapping occurred.**
The previous executable is also retained on `/state/bin` for rollback.

Public acceptance verified authenticated dashboard login, static assets,
anonymous API refusal, pairing, two released daemons, binary create/edit/delete,
history and rollback. It used a dedicated **Release acceptance 2026-11-24**
folder, leaving existing folders untouched. Its test file was deleted and
both temporary devices revoked; the dedicated folder retains test history.

Post-upgrade SQLite integrity and foreign-key checks passed. Every original
operation/version row, device/folder ID and chunk hash/size record remains.
All four original active devices remain active. Acceptance adds four history
rows, two chunks, one folder and two revoked device records; overwrite count
remains 60. No schema downgrade or history cleanup was attempted.

## Still open

This completes the hardening release and server rollout, not the broader M5
backlog. Submission deduplication, streaming large files, folder authorization,
integrity/repair tooling, quotas/retention/GC, mobile hardware validation,
generated DTOs, full-content verification and signed releases remain staged
follow-up work in [the release runbook](../releasing.md).

No real client installation/service was upgraded automatically. Client rollout
remains gradual; legacy installations need the documented manual swap once.
