# Stage 1: build the gait binary inside a Rust container
FROM rust:1.80-slim AS builder

# cmake: required by git2 vendored libgit2 build
# libssl-dev + pkg-config: required by reqwest (OpenSSL)
RUN apt-get update && apt-get install -y \
    pkg-config libssl-dev cmake gcc \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY gait-rs/ .
RUN cargo build --release

# Stage 2: minimal runtime image with tooling
FROM debian:bookworm-slim

# libssl3: required if reqwest is dynamically linked against OpenSSL
RUN apt-get update && apt-get install -y \
    git \
    ripgrep \
    fd-find \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && ln -s /usr/bin/fdfind /usr/local/bin/fd

COPY --from=builder /build/target/release/gait /usr/local/bin/gait
RUN chmod +x /usr/local/bin/gait

# Signals to the wrapper binary that we are inside the container — skip Docker re-routing
ENV GAIT_CONTAINER=1

ENTRYPOINT ["/usr/local/bin/gait"]
