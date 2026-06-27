# syntax=docker/dockerfile:1

# Public headless image for Driven: the debugging CLI (driven-cli) and the
# stress / chaos harness (driven-chaos), published to ghcr.io/pmaxhogan/driven.
# It deliberately does NOT build the Tauri desktop app (src-tauri) - that needs
# webkit2gtk + a GUI toolchain and is shipped as native installers instead.

# --- Builder ----------------------------------------------------------------
# rust:1-bookworm tracks the latest stable 1.x toolchain on Debian bookworm,
# matching the workspace's pinned stable channel (Cargo.toml rust-version 1.85).
FROM rust:1-bookworm AS builder

# sqlx compile-time-checked queries resolve against the committed .sqlx/ cache;
# there is no live DB in the image build (same contract as CI's SQLX_OFFLINE).
ENV SQLX_OFFLINE=true

WORKDIR /build

# Copy the whole workspace. A virtual-workspace build parses EVERY member
# manifest - including src-tauri/Cargo.toml - even when building only two
# crates, and SQLX_OFFLINE needs the .sqlx/ cache, so a partial copy breaks the
# build. .dockerignore trims target/, the ui build artifacts, and .git.
COPY . .

# Build ONLY the two headless binaries, never src-tauri. reqwest is configured
# rustls-only (Cargo.toml: rustls-tls-native-roots, no native-tls), so the
# builder needs no libssl-dev; the rust:1-bookworm image already ships cc.
RUN cargo build --release -p driven-cli -p driven-chaos

# --- Runtime ----------------------------------------------------------------
# bookworm-slim matches the builder's glibc exactly (the binaries link glibc),
# so there is no ABI surprise. Only ca-certificates is needed at runtime, for
# the outbound TLS roots reqwest/rustls uses to reach Google Drive.
FROM debian:bookworm-slim AS runtime

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/driven-cli /usr/local/bin/driven-cli
COPY --from=builder /build/target/release/driven-chaos /usr/local/bin/driven-chaos
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

# Run as a non-root user. Beyond the usual hardening, the chaos harness's
# permission-deny scenarios (noaccess-*, posix-mode-000) only engage when the
# process is NOT root - as root those denials are bypassed and the rows fail
# instead of exercising the deny path (this is also why GitHub's non-root CI
# runner passes them). A real user with a writable HOME + workdir is required:
# some scenarios write relative to cwd / HOME, which a bare numeric --user
# (cwd=/, no passwd entry) cannot.
RUN useradd --create-home --uid 10001 driven
USER driven
ENV HOME=/home/driven
WORKDIR /home/driven

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
