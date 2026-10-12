# syntax=docker/dockerfile:1
#
# TramaWorkspace collaboration service (`trama-collab`).
#
# Build from the repository root (Compose does this for you):
#   docker build -f deploy/Dockerfile -t trama-collab .
#
# Only `trama-collab` and `trama-collab-protocol` are compiled: the GPUI
# desktop app and the daemon are never built here. The image contains no
# secrets; the SQLite database lives on the /data volume.

ARG RUST_IMAGE=rust:1-slim-bookworm
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian12:nonroot

# -- build --------------------------------------------------------------------
FROM ${RUST_IMAGE} AS build
WORKDIR /src
ENV CARGO_TERM_COLOR=never \
    CARGO_PROFILE_RELEASE_STRIP=true \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1

# A workspace of just the two crates this image needs. The lockfile is the
# repository's: cargo prunes it to the packages these crates use and keeps
# their locked versions.
COPY Cargo.toml ./Cargo.toml.full
COPY Cargo.lock LICENSE ./
RUN sed 's|^members = .*|members = ["crates/trama-collab-protocol", "apps/trama-collab"]|' \
        Cargo.toml.full > Cargo.toml \
    && rm Cargo.toml.full \
    && grep -qx 'members = \["crates/trama-collab-protocol", "apps/trama-collab"\]' Cargo.toml

COPY crates/trama-collab-protocol/Cargo.toml crates/trama-collab-protocol/Cargo.toml
COPY apps/trama-collab/Cargo.toml apps/trama-collab/Cargo.toml

# Dependency layer: compile third-party crates against empty stubs so that a
# source-only change does not rebuild them.
RUN mkdir -p crates/trama-collab-protocol/src apps/trama-collab/src \
    && touch crates/trama-collab-protocol/src/lib.rs apps/trama-collab/src/lib.rs \
    && echo 'fn main() {}' > apps/trama-collab/src/main.rs \
    && cargo build --release -p trama-collab

COPY crates/trama-collab-protocol/src crates/trama-collab-protocol/src
COPY apps/trama-collab/src apps/trama-collab/src
RUN touch crates/trama-collab-protocol/src/lib.rs apps/trama-collab/src/lib.rs apps/trama-collab/src/main.rs \
    && cargo build --release -p trama-collab \
    && install -D target/release/trama-collab /out/usr/local/bin/trama-collab \
    # The data directory, owned by the runtime user (distroless "nonroot").
    && install -d -o 65532 -g 65532 -m 0700 /out/data

# -- runtime ------------------------------------------------------------------
FROM ${RUNTIME_IMAGE}
LABEL org.opencontainers.image.title="trama-collab" \
      org.opencontainers.image.description="TramaWorkspace collaboration service (plain HTTP; run it behind a TLS reverse proxy)" \
      org.opencontainers.image.source="https://github.com/alessandrobrunoh/trama-workspace"
COPY --from=build /out/usr/local/bin/trama-collab /usr/local/bin/trama-collab
COPY --from=build --chown=65532:65532 /out/data /data

# Where `serve` and the admin commands find the database. The listen address
# and the reverse-proxy opt-in are deliberately NOT set here: without them
# the service stays on loopback. See deploy/docker-compose.yml.
ENV TRAMA_COLLAB_DB=/data/collab.sqlite \
    PATH=/usr/local/bin:/usr/bin:/bin

USER 65532:65532
WORKDIR /data
VOLUME ["/data"]
EXPOSE 8787

# No shell and no curl in this image: the binary probes its own /healthz.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/trama-collab", "healthcheck"]

ENTRYPOINT ["/usr/local/bin/trama-collab"]
CMD ["serve"]
