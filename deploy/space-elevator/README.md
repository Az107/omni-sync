# Deploying the omni-sync server to space-elevator

The server runs as a container on a managed named volume, routed by
space-elevator's Traefik at:

**https://elevator.albruiz.dev/app/omni-sync/** (path prefix stripped).

## Two things that matter

1. **Ship a `compose.yml`, not a bare Dockerfile.** Archive/Dockerfile
   drops get `SyntheticCompose()` — one service, **no volumes** — and the
   server's whole job is holding state. A shipped compose file wins
   (`deployer.resolveCompose` → `FindComposeFile`) and gets platform
   managed named volumes, so the SQLite state and chunk store survive
   redeploys.

2. **Listen on port 80, as root inside the container.** SE's rootful
   Traefik reaches a host-published web app on the port its own nginx
   drops use. A non-root user cannot bind `:80`, so the container runs as
   root (SE drops the dangerous capabilities and sets
   `no-new-privileges`). The first deploy used 8787 as a non-root user and
   came up `502` through Traefik even though the container logged
   `listening on 0.0.0.0:8787`; port 80 routes.

## Driving space-elevator

SE runs as the `services` user. Use the `se` wrapper
(`/usr/local/bin/se`), which the `service-admin` group may run
non-interactively:

```sh
se apps list                       # read-only
se apps logs omni-sync --tail 50
```

The group is applied at login. In a shell that predates being added to
`service-admin`, refresh it per command:

```sh
sg service-admin -c 'se apps list'
```

## Deploy / update

The source is staged to a path `services` can read:

```sh
tar --exclude=./target --exclude=./.git -cf - . | (cd /tmp/omni-sync-se && tar xf -)
chmod -R a+rX /tmp/omni-sync-se
```

First deploy:

```sh
sg service-admin -c "se apps upload /tmp/omni-sync-se --kind custom --name omni-sync \
  --secret OMNI_SYNC_PASSWORD='<password>'"
```

Update an existing app (rebuilds, preserves volumes):

```sh
sg service-admin -c 'se apps update omni-sync --archive /tmp/omni-sync-se --yes'
```

`--kind custom` requires a Dockerfile; with `compose.yml` present the
platform uses the compose stack.

## Updating the server binary from GitHub

The container's `docker-entrypoint.sh` prefers a binary on the `/state`
volume. To move to a published release without rebuilding the image:

```sh
sg service-admin -c 'se apps exec omni-sync -- /usr/local/bin/docker-entrypoint.sh --help'  # sanity
# inside the container (or via `se apps exec`):
omni-sync-server update --check
omni-sync-server update --download-only /state/bin
# restart so the entrypoint picks up the staged binary:
sg service-admin -c 'se apps restart omni-sync'
```

`update` downloads the release asset for `x86_64-unknown-linux-gnu`,
verifies it against the release's `SHA256SUMS`, and writes it to
`/state/bin/omni-sync-server` (executable). The staged binary survives
redeploys because `/state` is a managed volume; delete it to return to
the image's binary after a `se apps update`.

**The repository is public**, so the updater reads releases
unauthenticated and no token is needed. If it goes private again, set
`OMNI_SYNC_GITHUB_TOKEN` for the container (compose passes it through
when the host environment has it).

## Verify from the outside

```sh
curl -sS https://elevator.albruiz.dev/app/omni-sync/healthz          # {"ok":true,...}
curl -sS -o /dev/null -w '%{http_code}\n' https://elevator.albruiz.dev/app/omni-sync/   # 303
```

Log in with `OMNI_SYNC_PASSWORD` and confirm the overview renders.

## Enroll clients

```sh
# dashboard → devices → Add a device → copy the code
omni-sync enroll --server https://elevator.albruiz.dev/app/omni-sync --code ABCD-EFGHJ
omni-sync folders
omni-sync add <folder-id> ~/Documents/Shared
omni-sync daemon
```

The client appends API paths *under* its base URL, so the `/app/omni-sync`
prefix needs no special configuration.

## Persistence — space-elevator does not carry volumes across updates

- `omnidata` → `/data` (content-addressed chunk store)
- `omnistate` → `/state` (SQLite: devices, folders, ops, versions, overwrites)

**Every `se apps update` mounts brand-new, empty volumes.** space-elevator
v0.1.0 manages app volumes itself — it strips the compose's `volumes:`
block and records the mapping in its `app_storage` table — but that table
is **empty for every app on this host**, and there is no storage flag on
`deploy`/`upload`/`update`. So each release gets fresh, randomly named
volumes (e.g. `49999d76…` → `/state`), and the server comes up with an
empty database and chunk store. The volume name is not derivable from the
app, release, or logical name, and the compose content makes no
difference, so this cannot be pinned from here.

The update still prints "persistent data was preserved" and writes a
backup under `~services/.local/share/space-elevator/backups/<app-id>/<release>/`,
but that manifest is metadata-only (`"storage": null`): it does not
snapshot the volume. The previous releases' volumes are **not deleted**,
so the data is recoverable.

To carry it across after a `se apps update`:

```sh
sg service-admin -c 'se apps stop omni-sync'
# The new container's volumes:
sudo -n -u services -- env HOME=/home/services XDG_RUNTIME_DIR=/run/user/1004 \
  podman inspect se-omni-sync-web \
  --format '{{range .Mounts}}{{.Name}} -> {{.Destination}}{{"\n"}}{{end}}'
# The previous release's volumes still hold the data:
sudo -n -u services -- ls /home/services/.local/share/containers/storage/volumes/
# Fold the old WAL into a single clean DB, then copy server.db and chunks/
# into the new volumes (owned services:services, DB mode 0600), and:
sg service-admin -c 'se apps start omni-sync'
```

### The update path that does not lose data

For a **binary-only** update, do not use `se apps update`. Every dashboard
template and font is embedded in the binary (rust-embed), so swapping the
binary carries the UI too. Publish a release, then update inside the
running container, which writes to the volume it already holds:

```sh
sudo -n -u services -- env HOME=/home/services XDG_RUNTIME_DIR=/run/user/1004 \
  podman exec se-omni-sync-web \
  omni-sync-server update --download-only /state/bin
sg service-admin -c 'se apps restart omni-sync'
```

`docker-entrypoint.sh` runs `/state/bin/omni-sync-server` when present, and
`stop`/`start`/`restart` reuse the same container and its volumes, so
nothing is remapped. `se apps update` is only needed when the image or
toolchain must change — and then carry the volumes across as above.

The runtime base is `debian:trixie-slim` on purpose: the CI-built release
binary is linked against Ubuntu 24.04's glibc (2.39), which bookworm (2.36)
cannot run, so an in-container update on bookworm would crash-loop with
`GLIBC_2.38 not found`. The baked binary is built on bookworm and runs on
trixie regardless. The same glibc floor applies to the **client** binaries:
a Linux older than Ubuntu 24.04 / Debian 13 may refuse them, so build them
on an older runner if that matters.

Keep the logical volume names (`omnidata`, `omnistate`) and the service
name (`web`) unchanged: the compose still has to declare them for the
first deploy.


## Status

**0.4.0 verified from the outside.**

**Client 0.4.1 shipped; the server stays at 0.4.0 on purpose.** 0.4.1 is a
client-only change (daemon service install + restart-on-update), so the wire
contract is unchanged (`PROTOCOL_VERSION` is still 1) and the deployed server
needs no redeploy. `healthz` therefore keeps reporting `0.4.0`, which is
accurate — the `omni-sync-server-0.4.1-*` asset exists only for someone who
later wants the version strings to line up.

- `GET /healthz` → `{"ok":true,"version":"0.4.0"}`
- `GET /` → `303` to login; login renders the designed dashboard;
  `/devices` mints a pairing code; `/static/app.css` → `200`.
- `GET /api/v1/folders/{id}/ops/stream` → `401` without a device token,
  which is how the op-log SSE endpoint confirms it is routed.
- The 0.3.0 → 0.4.0 update was done in-container
  (`omni-sync-server update --download-only /state/bin`, then
  `se apps restart omni-sync`). PID 1 then ran `/state/bin/omni-sync-server`
  and `healthz` reported 0.4.0. No server migration ran (the new migration
  is agent-side only), and `doctor` still listed the same 4 devices and
  state dir, so the restart reused the container and its volumes as
  intended. The release assets are unauthenticated now that the repository
  is public.

**The release must publish the host's own architecture.** This host and the
container are `aarch64` and the running binary reports
`aarch64-unknown-linux-gnu`, so `update` downloads
`omni-sync-server-<version>-aarch64-unknown-linux-gnu`. An x86_64-only
server matrix would leave the in-container update with no asset to fetch.

The port-80/root detail above was found the hard way: the 8787 non-root
build reported `running` yet returned `502`, which is exactly the failure
mode space-elevator's own AGENTS.md warns about.
