# Stage 1: build the gait binary inside a Rust container
FROM rust:1.80-slim AS builder

# cmake + pkg-config + libssl-dev: required by git2 vendored libgit2 (needs OpenSSL for HTTPS transport)
# gcc is already present in rust:slim — not re-listed
RUN apt-get update && apt-get install -y \
    cmake pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY gait-rs/ .
RUN cargo build --release --locked

# Stage 2: minimal runtime image with tooling
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    git \
    ripgrep \
    fd-find \
    ca-certificates \
    jq \
    tini \
    && rm -rf /var/lib/apt/lists/* \
    && ln -s /usr/bin/fdfind /usr/local/bin/fd

# Agent identity for commits; safe.directory for bind-mounted workspaces
# (git 2.35.2+ rejects repos owned by a different UID without this)
RUN git config --system user.name "gait" \
 && git config --system user.email "gait@localhost" \
 && git config --system --add safe.directory '*'

COPY --from=builder /build/target/release/gait /usr/local/bin/gait

# C.UTF-8 is built into glibc — no locales package needed
ENV LANG=C.UTF-8 \
    LC_ALL=C.UTF-8 \
    GAIT_CONTAINER=1

# tini as PID 1: clean signal handling and zombie reaping for short-lived shell subprocesses
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/gait"]
