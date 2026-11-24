# Client 0.4.2 / server 0.4.1 — hardening release

Both components retain protocol version 1. They release independently as
`client-v0.4.2` and `server-v0.4.1`; each release contains only its component.

## Changes

- Confine receiving filesystem access to directory capabilities; validate
  remote paths again on the client and preserve local edits during replay.
- Drain bounded historical operation pages before the first push and adopt
  matching existing files without generating false overwrites.
- Share an OS ownership lock between daemons and one-shot client engines.
- Validate submitted batches atomically, including chunk availability and
  lengths, and prevent cross-folder restore references.
- Parse enrollment callbacks as URLs, harden dashboard redirects, expire
  sessions server-side, and bound authentication attempts and password work.
- Bound control/HTTP/SSE I/O and harden atomic binary installation.
- Include shared folder policy, paged APIs, deleted-file recovery and device
  management from the previously unreleased Obsidian interoperability work.

## Upgrade notes

**Clients on 0.4.1 or earlier need one manual upgrade.** Their legacy updater
cannot follow `client-v*` tags. Download the archive for your OS/architecture,
verify its SHA-256 against `SHA256SUMS`, stop the daemon, replace the binary,
and restart it. Subsequent `omni-sync update` calls follow the client train.

**The deployed 0.4.0 server also needs a manual upgrade once.** Its parser
cannot read `server-v*` tags, even with `--to`. Download and checksum-verify
the ARM64 bare binary outside the old updater, preserve the running binary,
and atomically stage the verified executable at `/state/bin/omni-sync-server`
in the existing container. Then restart that container. Subsequent server
updates can use `update --download-only /state/bin`. Back up the database and
chunk store first; **do not use `se apps update`**, which can replace
persistent volumes on this host.

Server schema migration `0003` adds folder policy. Migrations are forward-only:
binary rollback is not a database downgrade. Keep a consistent pre-upgrade
backup for disaster recovery and avoid losing edits made after that backup.

Enrollment still grants access to **all folders**; membership is not an
authorization boundary. Large files still require whole-file buffering.
Retention/GC, quotas, retry deduplication and release signatures are not part
of this patch. Read the
[hardening notes](https://github.com/Az107/omni-sync/blob/main/docs/hardening.md)
before deployment.
