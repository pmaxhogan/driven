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

# --- E2E builder ------------------------------------------------------------
# Builds the FULL Tauri desktop app (src-tauri -> driven-app) for Linux plus
# the app-level WebDriver suite (driven-e2e) and tauri-driver. This stage is
# NOT part of the default image (the last stage below stays the default
# `docker build` target); build it explicitly with `--target e2e-runtime`.
FROM rust:1-bookworm AS e2e-builder

ENV SQLX_OFFLINE=true

# Tauri 2 Linux build deps (webkit2gtk 4.1 on bookworm - the same set ci.yml
# installs on ubuntu) + Node 22 / pnpm via corepack for the ui build that
# `frontendDist` embeds into the binary.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      libwebkit2gtk-4.1-dev libgtk-3-dev libxdo-dev libssl-dev \
      libayatana-appindicator3-dev librsvg2-dev libsoup-3.0-dev \
      curl ca-certificates \
 && curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
 && apt-get install -y --no-install-recommends nodejs \
 && corepack enable \
 && rm -rf /var/lib/apt/lists/*

# tauri-driver proxies the WebDriver protocol to WebKitWebDriver and launches
# the app binary per session. Small crate; its own layer so it caches.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install tauri-driver --locked

WORKDIR /build
COPY . .

# The webview assets Tauri embeds at compile time (`frontendDist: ../ui/dist`).
# Run from INSIDE ui/ so corepack resolves the `packageManager` pin in
# ui/package.json (invoking `pnpm --dir ui` from the repo root makes corepack
# refuse to version-switch and fail the build).
RUN --mount=type=cache,target=/build/ui/node_modules \
    cd ui && pnpm install --frozen-lockfile && pnpm build

# driven-app MUST be a RELEASE build: tauri-build only embeds the ui/dist
# assets (frontendDist) in non-dev profiles - a dev-profile binary points the
# webview at the devUrl (localhost:5173) and renders nothing in the container.
# The harness + CLI stay on the dev profile (debug info stripped, the CI
# trick) for build speed. Cache mounts keep the registry + target across image
# rebuilds; binaries are copied OUT of the cached target dir so later stages
# can COPY them.
ENV CARGO_PROFILE_DEV_DEBUG=0
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release -p driven-app --features driven-app/custom-protocol \
 && cargo build -p driven-e2e -p driven-cli \
 && mkdir -p /out \
 && cp target/release/driven-app target/debug/driven-e2e target/debug/driven-cli /out/

# --- E2E runtime ------------------------------------------------------------
# The agent playground: the real desktop app under Xvfb + WebKitWebDriver +
# tauri-driver, with the tooling for network / filesystem / permission fault
# injection. Built explicitly via `--target e2e-runtime` (see justfile `e2e-*`
# recipes); never the default target.
FROM debian:bookworm-slim AS e2e-runtime

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates \
      # webview + app runtime libs
      libwebkit2gtk-4.1-0 libgtk-3-0 libayatana-appindicator3-1 librsvg2-2 \
      # headless display + WebDriver backend
      xvfb xauth webkit2gtk-driver \
      # secret-service so the OS-keychain seam (`keyring`) works headless
      dbus dbus-x11 gnome-keyring libsecret-1-0 \
      # fault injection + debugging tools for agents
      iproute2 iptables sudo procps curl jq sqlite3 \
 && rm -rf /var/lib/apt/lists/*

# MinIO (a real S3 destination for round-trip + wire-fault scenarios) and
# toxiproxy (the network-fault proxy the suite parks between the app and
# MinIO: latency, bandwidth, timeouts, hard cuts). Arch-aware: the image
# builds arm64-native on the M5 dev machine and amd64 in CI.
ARG TARGETARCH
RUN set -eux; \
    case "${TARGETARCH}" in \
      arm64) MINIO_ARCH=linux-arm64; TOXI_ARCH=linux-arm64 ;; \
      *)     MINIO_ARCH=linux-amd64; TOXI_ARCH=linux-amd64 ;; \
    esac; \
    curl -fsSL "https://dl.min.io/server/minio/release/${MINIO_ARCH}/minio" -o /usr/local/bin/minio; \
    curl -fsSL "https://dl.min.io/client/mc/release/${MINIO_ARCH}/mc" -o /usr/local/bin/mc; \
    curl -fsSL "https://github.com/Shopify/toxiproxy/releases/download/v2.12.0/toxiproxy-server-${TOXI_ARCH}" -o /usr/local/bin/toxiproxy-server; \
    curl -fsSL "https://github.com/Shopify/toxiproxy/releases/download/v2.12.0/toxiproxy-cli-${TOXI_ARCH}" -o /usr/local/bin/toxiproxy-cli; \
    chmod +x /usr/local/bin/minio /usr/local/bin/mc /usr/local/bin/toxiproxy-server /usr/local/bin/toxiproxy-cli

COPY --from=e2e-builder /out/driven-app /usr/local/bin/driven-app
COPY --from=e2e-builder /out/driven-e2e /usr/local/bin/driven-e2e
COPY --from=e2e-builder /out/driven-cli /usr/local/bin/driven-cli
COPY --from=e2e-builder /usr/local/cargo/bin/tauri-driver /usr/local/bin/tauri-driver
COPY docker/e2e-entrypoint.sh /usr/local/bin/e2e-entrypoint.sh
RUN chmod +x /usr/local/bin/e2e-entrypoint.sh

# Non-root user (permission-denial scenarios need a non-root subject), with
# passwordless sudo so the suite / an agent can flip network faults (iptables,
# tc) and permission fixtures (chown) from inside the container.
RUN useradd --create-home --uid 10001 driven \
 && echo "driven ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/driven-e2e \
 && chmod 0440 /etc/sudoers.d/driven-e2e \
 # Xvfb cannot create the X11 socket dir as a non-root user; pre-create it
 # with the sticky-tmp mode a real session would have.
 && mkdir -p /tmp/.X11-unix \
 && chmod 1777 /tmp/.X11-unix
USER driven
ENV HOME=/home/driven
WORKDIR /home/driven

# The app is launched BY tauri-driver (per WebDriver session); these defaults
# make every launch hermetic + observable.
# NOTE: DRIVEN_USE_FAKE_REMOTE is deliberately NOT set image-wide: the suite
# sets it PER SCENARIO on the tauri-driver it spawns (a leaked image-level =1
# silently gave real-backend scenarios the fake remote - every localfs/S3 row
# uploaded into memory and the destination never filled).
ENV DRIVEN_E2E_WEBDRIVER_URL=http://127.0.0.1:4444 \
    DRIVEN_E2E_APP_BINARY=/usr/local/bin/driven-app \
    DISPLAY=:99

ENTRYPOINT ["/usr/local/bin/e2e-entrypoint.sh"]

# --- Runtime ----------------------------------------------------------------
# bookworm-slim matches the builder's glibc exactly (the binaries link glibc),
# so there is no ABI surprise. Only ca-certificates is needed at runtime, for
# the outbound TLS roots reqwest/rustls uses to reach Google Drive.
# NOTE: keep this stage LAST - it is the default `docker build` target the
# docker.yml publish workflow builds.
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
