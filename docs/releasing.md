# Release runbook

## Before tagging

1. Review changes; update `plan.md`, `AGENTS.md` and release notes. Versions
   belong to each binary's manifest; shared-library changes require both
   binaries to ship. Never move a published tag or replace published assets.
2. Run locally:

   ```sh
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets --locked -- -D warnings
   cargo test --workspace --locked
   python3 -m unittest discover -s scripts -p 'test_*.py'
   cargo build --locked -p omni-cli -p omni-server
   python3 scripts/smoke.py
   python3 scripts/release.py client-v0.4.2
   python3 scripts/release.py server-v0.4.1
   ```

   The Linux socket smoke test retains isolated state/logs in `/tmp/opencode`.
   It never touches real user config or installs a service. Unit tests remain
   hermetic; the smoke test is a separate CI job.
3. Push the candidate commit and require **all CI jobs**, including native
   Linux/macOS/Windows tests and the smoke test, to pass on that exact commit.
   Do not tag based only on local Linux results.
4. For this hardening release, publish annotated `client-v0.4.2` and
   `server-v0.4.1` tags on the validated commit. The Release workflow checks
   out the exact tag (also for manual dispatch), checks manifest agreement,
   tests native platforms and verifies native binary versions. The release
   contract in `scripts/release.py` defines six client/two server targets.
5. The publisher validates every archive against its bare binary, generates
   complete checksums, uploads a draft, downloads and revalidates its assets,
   then publishes it. It refuses to overwrite an already published release.
   Failed drafts can be retried; do not change their tags. Neither component
   is marked repository-wide `latest`, because legacy clients cannot parse
   component tags. Before future releases, update the workflow's notes file.

## After publication

- Download native artifacts; verify `SHA256SUMS`, `--version`, and archive
  contents. The workflow does not execute cross-built macOS x64 or Windows
  ARM64 binaries; test on corresponding hardware when available.
- Use an isolated copy of the prior client to test update discovery,
  installation and restart. Legacy clients need a manual swap once. Test
  `--check` for both component trains, not `/releases/latest`.
- Do not automatically downgrade or replace an existing installed client.
  Roll out gradually and check daemon status/logs after restart.

## Production server

Follow [`deploy/space-elevator/README.md`](../deploy/space-elevator/README.md).
Before updating, record the running version and volume mapping, preserve the
running binary, and take a consistent database/chunk-store backup. A short
server stop while snapshotting both volumes is the simplest consistent
backup; treat its contents as secrets. Verify the backup before proceeding.

The deployed 0.4.0 server's legacy updater cannot parse `server-v*` tags,
including explicit `--to` requests. Bootstrap this first upgrade manually:
download the ARM64 bare binary and `SHA256SUMS`, verify the exact checksum,
copy to a unique staging name on `/state/bin`, chmod executable, and rename
over the staged server only after retaining its previous binary. Do not use
the old updater for this transition.

Stage the published ARM64 binary on the **existing** `/state` volume and
restart the existing container. Do not rebuild/redeploy with `se apps update`.
Check public health/version, login, static assets, unauthorized API refusal,
pairing, two-client create/edit/delete, history and restore. Compare device,
folder and history counts with the pre-upgrade baseline. Keep the previous
binary/backup until acceptance is complete.

If startup fails, preserve logs and restore the previous staged binary;
restart the same container. Do not reverse migrations or overwrite the live
database from an old backup unless explicitly recovering from a failed
migration and accounting for any intervening writes.

## Subsequent milestones (not patch-release blockers)

1. Durable operation IDs/retry deduplication and bounded-memory streaming.
2. Admin-controlled grants/invitations and authorization, including chunks.
3. Read-only integrity/accounting, dry-run maintenance, explicit repair, safe
   retention/GC, then atomic quota enforcement.
4. Physical mobile tests/generated DTOs, full-content verification, signed
   releases/key rotation and Windows updater recovery/cleanup.

Use minor versions for user-facing or protocol-affecting capabilities and
decide backward compatibility explicitly before each milestone.
