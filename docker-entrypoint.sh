#!/bin/sh
# Prefer a binary installed onto the state volume by `omni-sync-server update
# --download-only /state/bin`; fall back to the one baked into the image.
#
# The volume copy survives redeploys, so this is what lets the server update
# itself from GitHub without rebuilding the image. Delete the staged file to
# return to the image's binary (e.g. after a `se apps update`).
set -e

STAGED=/state/bin/omni-sync-server
if [ -x "$STAGED" ]; then
  exec "$STAGED" "$@"
fi

exec /usr/local/bin/omni-sync-server "$@"
