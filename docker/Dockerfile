# syntax=docker/dockerfile:1
#
# Tektii Gateway — multi-stage Docker build
#
# Default:     docker build -f docker/Dockerfile -t tektii/gateway:0.1.0 .
# Distroless:  docker build -f docker/Dockerfile --build-arg BASE=distroless -t tektii/gateway:0.1.0-slim .
#
# Runtime hardening (apply via docker run or compose):
#   --read-only --cap-drop=ALL --security-opt=no-new-privileges

ARG BASE=debian

# =============================================================================
# Stage 1: Chef base — install cargo-chef and mold linker
# =============================================================================
FROM rust:1.91-bookworm AS chef

RUN apt-get update && apt-get install -y mold clang && rm -rf /var/lib/apt/lists/* \
    && cargo install cargo-chef --locked

ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=clang \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-fuse-ld=mold" \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=clang \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-fuse-ld=mold"

WORKDIR /app

# =============================================================================
# Stage 2: Planner — compute dependency recipe
# =============================================================================
FROM chef AS planner

COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
COPY src/ ./src/

RUN cargo chef prepare --recipe-path recipe.json

# =============================================================================
# Stage 3: Builder — cook deps then build release binary
# =============================================================================
FROM chef AS builder

ARG FEATURES=

COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json -p tektii-gateway ${FEATURES}

COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
COPY src/ ./src/

RUN cargo build --release -p tektii-gateway ${FEATURES}

# =============================================================================
# Stage 4a: Runtime — Debian slim (default)
# =============================================================================
FROM debian:bookworm-slim AS runtime-debian

RUN apt-get update && apt-get install -y --no-install-recommends \
        libssl3 ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system gateway && useradd --system --gid gateway gateway

USER gateway

# =============================================================================
# Stage 4b: Runtime — Distroless
# =============================================================================
FROM gcr.io/distroless/cc-debian12 AS runtime-distroless

USER nonroot

# =============================================================================
# Stage 5: Final — select runtime via BASE arg
# =============================================================================
# hadolint ignore=DL3006
FROM runtime-${BASE} AS final

COPY --from=builder /app/target/release/tektii-gateway /usr/local/bin/tektii-gateway

ARG BUILD_DATE
ARG GIT_SHA
ARG VERSION

LABEL org.opencontainers.image.created="${BUILD_DATE}" \
      org.opencontainers.image.revision="${GIT_SHA}" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.title="Tektii Gateway" \
      org.opencontainers.image.description="Unified trading API gateway — REST + WebSocket"

EXPOSE 8080

CMD ["/usr/local/bin/tektii-gateway"]
