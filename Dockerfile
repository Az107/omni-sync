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
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home omni
COPY --from=build /src/target/release/omni-sync-server /usr/local/bin/omni-sync-server

# Data (chunks) and state (SQLite) live on volumes, not the container layer.
ENV OMNI_SYNC_ADDR=0.0.0.0:8787 \
    OMNI_SYNC_DATA_DIR=/data \
    OMNI_SYNC_STATE_DIR=/state
RUN mkdir -p /data /state && chown -R omni:omni /data /state
VOLUME ["/data", "/state"]
EXPOSE 8787
USER omni
ENTRYPOINT ["/usr/local/bin/omni-sync-server"]
CMD ["serve"]
