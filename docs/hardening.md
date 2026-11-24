# Hardening and trust boundaries

## Deployment assumptions

- Terminate public traffic with HTTPS at the reverse proxy. The server's
  HTTP listener is not a public TLS endpoint. Do not send device tokens or
  dashboard passwords over untrusted plaintext connections.
- Behind a trusted TLS-terminating proxy, set
  `OMNI_SYNC_TRUST_FORWARDED_PROTO=1` (or `trust_forwarded_proto = true` in
  `server.toml`) so HTTPS dashboard cookies carry `Secure`. This is opt-in:
  block direct access to the application listener and configure the proxy to
  overwrite `X-Forwarded-Proto` and append the verified client IP to
  `X-Forwarded-For`, not blindly pass through client-supplied values.
  Do not enable it merely because an arbitrary request contains that header.
- Pair only trusted devices. **Enrollment currently grants access to all
  folders**, including their policies and retained history. Membership is
  recorded for coordination, not enforced as authorization. Registering a
  member is self-service, so simply adding a membership check would not
  create a secure invitation system.
- Treat the configuration, client database, server database, and chunk
  directories as private application state. Do not place them in a shared
  sync root or give other users write access to their parent directories.
  Keep the client database and its ownership lock on a local filesystem with
  reliable OS locking; advisory locks are not a guarantee on every network
  filesystem.
- Keep backups independently of sync. Last-write-wins retains remote
  history, but sync is not a substitute for an offline backup or a
  consistent SQLite/chunk-store backup.

## Regression coverage

The workspace test suite is hermetic: it uses temporary directories and
in-process HTTP requests rather than real sockets or external services.

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
```

Filesystem confinement must be tested at the **receiving client** as well
as at the server. A client can encounter old invalid history or a malicious
server even when a newly deployed server rejects unsafe operations.

Keep regression coverage for these boundaries:

- Traversal, absolute paths, platform-specific path prefixes, and symlink
  parents must not permit writes or deletes outside a folder.
- A pre-existing temporary-file symlink must not redirect a write.
- A local first-sync conflict must survive multiple historical puts and
  deletes until its content is uploaded.
- Incoming operations must not silently replace unsynced local edits.
- HTTP chunk bodies are limited to 2 MiB (the default chunker emits at most
  1 MiB); clients reject larger uploads locally rather than relying on an
  opaque body-extractor error from the server.
- Invalid or missing chunks must not create committed, un-downloadable
  versions; an invalid batch must not partially commit.
- Enrollment callbacks must be parsed URLs, not string-prefix matches.
- Daemons and one-shot engines must share exclusive database ownership.
- Expired dashboard sessions must be rejected by the server, independently
  of whether a browser still sends the cookie.

Login and enrollment each use a per-source-IP attempt budget. Without proxy
trust, the source comes from the TCP peer, not forwarding headers. With proxy
trust, only the proxy-appended last IP is used, not an attacker-controlled
earlier hop. An embedding that does not supply connection information shares
an `unknown` budget. Password hashing also has a fail-fast concurrency bound whose permits
remain held until the blocking work finishes, even if a request disconnects.

## Remaining design work

- **Folder authorization:** requires an administrator-controlled grant or
  invitation model, authorization on all related endpoints, and a policy
  for content-addressed chunks shared across folders.
- **Exactly-once submissions:** atomic batches prevent partial acceptance,
  but a lost response after a successful commit still needs durable
  operation identifiers and retry deduplication for exactly-once behavior.
- **Large files:** file chunking/assembly still needs a complete streaming
  design to guarantee constant memory for very large files.
- **Storage lifecycle:** quotas, retention enforcement, chunk garbage
  collection, and repair of previously stored invalid history need their
  own operational tooling. New request validation does not repair an old
  database automatically.
- **Release authenticity:** updater checksums detect corruption, but the
  asset and checksum share the same release publisher. Independent release
  signatures are not implemented.
- **Windows updater cleanup:** Windows may prevent deletion of a backup of a
  still-running executable. A uniquely named old binary can remain alongside
  the installation until that process exits; failed rollback reports the
  backup path rather than deleting another updater's copy.
- **Local concurrent writers:** an ordinary editor does not participate in
  the engine's ownership lock. Filesystem operations cannot make a whole
  sync cycle transactional with arbitrary applications; use `on_close`
  and process holds for applications that keep mutable files open.
- **Unreadable inbound targets:** ordered inbound replay fails closed at a
  local target that cannot be read or safely represented (including a
  file-versus-directory collision). It does not acknowledge an operation it
  could not preserve/apply. Correct that target using the daemon's path/error
  warning to resume inbound replay for the folder. Outbound scans defer bad or
  unstable local paths without deleting their baselines, so other uploads can
  still proceed.

Run native macOS and Windows tests before releasing filesystem changes;
Linux tests alone do not validate those operating systems' rename,
permission, reparse-point, or file-locking semantics.
