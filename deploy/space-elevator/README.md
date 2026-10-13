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

**While the GitHub repo is private**, the updater needs a token to read
the release. Set `OMNI_SYNC_GITHUB_TOKEN` for the container (compose
passes it through when the host environment has it). Once the repo is
public, unset it.

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

## Persistence — and the compose-hash footgun

- `omnidata` → `/data` (content-addressed chunk store)
- `omnistate` → `/state` (SQLite: devices, folders, ops, versions, overwrites)

**space-elevator names these volumes by a hash of the compose stack, not
by the logical volume name.** Editing `compose.yml` — even to add a
`restart:` policy or an environment variable — changes the hash, so the
next `se apps update` points the app at *new, empty* volumes and the data
looks gone.

The update prints "persistent data was preserved" and writes a backup
under `~services/.local/share/space-elevator/backups/<app-id>/<release>/`,
but that manifest is metadata-only (`"storage": null`): it does **not**
snapshot the volume. The old volumes are not deleted, so the data is still
on disk and can be carried across:

```sh
# find the old volumes (64-hex names); the old pair predates this deploy
sudo -n -u services -- podman volume ls -q
# checkpoint the old DB so its WAL is folded in, then copy server.db and
# chunks/ into the volumes the *new* container mounts (podman inspect),
# with the container stopped, and start it again.
```

The safe rule: **treat `compose.yml` as frozen.** Change it as rarely as
possible, and when you must, plan to move the volume contents by hand.

Keep the logical volume names (`omnidata`, `omnistate`) and the service
name (`web`) unchanged regardless: they are what the compose stack
declares.


## Status

**Verified from the outside.**

- `GET /healthz` → `{"ok":true,"version":"0.1.0"}`
- `GET /` → `303` to login; login renders the dashboard; `/devices` mints
  a pairing code; `/static/app.css` → `200`.
- A real client enrolled against the public URL, joined a folder, and
  pushed a file; `GET /api/v1/folders/{id}/manifest` over the public URL
  showed it with the expected size and content hash.

The port-80/root detail above was found the hard way: the 8787 non-root
build reported `running` yet returned `502`, which is exactly the failure
mode space-elevator's own AGENTS.md warns about.
