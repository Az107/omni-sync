# Multi-stage build for the omni-sync server.
#
# Images are fully qualified (`docker.io/...`) so rootless Podman with no
# short-name registries still resolves them.

# ---- build -----------------------------------------------------------------
# rust:1.89 matches rust-toolchain.toml. cmake + build-essential are needed by
# the TLS stack (aws-lc-rs) and rusqlite's bundled SQLite (C).
FROM docker.io/library/rust:1.89-slim-bookworm AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential cmake pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p omni-server

# ---- runtime ---------------------------------------------------------------
FROM docker.io/library/debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/omni-sync-server /usr/local/bin/omni-sync-server

# Data (chunks) and state (SQLite) live on volumes, not the container layer.
#
# Listens on :80 — the port space-elevator's rootful Traefik is set up to reach
# for a host-published web app (its own nginx drops use 80). A non-root user
# cannot bind :80 here, so the process runs as root *inside* the container;
# space-elevator drops the dangerous capabilities and sets no-new-privileges,
# and nothing is exposed except through Traefik.
ENV OMNI_SYNC_ADDR=0.0.0.0:80 \
    OMNI_SYNC_DATA_DIR=/data \
    OMNI_SYNC_STATE_DIR=/state
RUN mkdir -p /data /state
VOLUME ["/data", "/state"]
EXPOSE 80
ENTRYPOINT ["/usr/local/bin/omni-sync-server"]
CMD ["serve"]
