# Deploying the omni-sync server to space-elevator

The server is a single container on a managed named volume, routed by
space-elevator's Traefik at `https://elevator.albruiz.dev/app/omni-sync/`
(path prefix stripped, so the app sees `/`).

A compose stack is required, not a plain Dockerfile drop: archive/Dockerfile
drops get a **synthetic** single-service compose with no volumes, and the
server's whole job is holding state (SQLite + the chunk store). Shipping
`compose.yml` makes the platform use **managed named volumes**
(`se-vol-<app-id>-<hash>`), which survive redeploys.

## One-time: staging copy

`space-elevator` runs as the `services` user and must read the source. Build
contexts are staged at `~/apps`, so an accessible copy is enough:

```sh
sudo su services
space-elevator apps upload /tmp/omni-sync-se --kind custom --name omni-sync \
  --secret OMNI_SYNC_PASSWORD='<choose-a-strong-password>'
```

Watch it:

```sh
space-elevator apps list            # until omni-sync shows running
space-elevator apps logs omni-sync --tail 50
```

## Verify from the outside

Do not trust "running". Hit it through Traefik:

```sh
curl -sS https://elevator.albruiz.dev/app/omni-sync/healthz
# {"ok":true,"version":"0.1.0"}

curl -sS -o /dev/null -w '%{http_code}\n' https://elevator.albruiz.dev/app/omni-sync/
# 303 (redirect to the login page)
```

Then log in at that URL with `OMNI_SYNC_PASSWORD` and confirm the overview
renders. If the app is `502`/`503`, the container is up but the route or the
port is wrong — check `apps logs`.

If the secret was not injected as an environment variable, the login page will
say "no dashboard password is set". Set it explicitly instead:

```sh
space-elevator apps upload /tmp/omni-sync-se --kind custom --name omni-sync \
  --env OMNI_SYNC_PASSWORD='<password>'
```

## Enroll clients

```sh
# dashboard → Devices → Add a device → copy the code
omni-sync enroll --server https://elevator.albruiz.dev/app/omni-sync --code ABCD-EFGHJ
omni-sync folders
omni-sync add <folder-id> ~/Documents/Shared
omni-sync daemon
```

The client appends API paths *under* the base URL, so the `/app/omni-sync`
prefix works without any special configuration.

## Optional: a dedicated domain

The plan's target is `sync.albruiz.dev` at the root of its own host. Attach it
in the dashboard (or `POST /api/v1/apps/omni-sync/domains`), add the DNS record,
and enroll with `--server https://sync.albruiz.dev`. No app change is needed.

## Persistence

- `omnidata` → `/data` (content-addressed chunk store)
- `omnistate` → `/state` (SQLite: devices, folders, ops, versions, overwrites)

Both are platform-managed volumes, backed up before an update and restored if
the new release fails to start. A redeploy keeps them as long as the logical
volume names (`omnidata`, `omnistate`) do not change.

## Updating

```sh
# replace the staged copy, then:
space-elevator apps update omni-sync /tmp/omni-sync-se
# or from an archive:
space-elevator apps update omni-sync /tmp/omni-sync-se.tar.gz
```

## Status

Container build and runtime were verified **locally** (Podman on this host):
image builds, `/healthz` answers, the dashboard login works, the `pair`
subcommand runs in-container, and `/state` persists as uid `omni`. The
space-elevator deploy itself is **unverified** here — this shell cannot reach
the `services` user (`sudo`/`su` require a password that is not available
non-interactively), so the `apps upload` step must be run by an operator.
