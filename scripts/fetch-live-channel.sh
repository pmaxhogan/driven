#!/usr/bin/env bash
# fetch-live-channel.sh (SPEC s15.3 / ROADMAP M9 R1-P1-7).
#
# Cloudflare Pages `pages deploy <dir>` publishes a WHOLE-SITE snapshot: it
# replaces every file under driven.maxhogan.dev/updates with the contents of the
# deployed dir. Each channel workflow (release.yml -> stable, dev-channel.yml ->
# dev) only generates ITS OWN channel's manifests, so deploying that alone would
# WIPE the other channel's live manifests.
#
# This script preserves the OTHER channel: it downloads that channel's
# currently-live per-platform `update.json` files from the live site and writes
# them into the local tree about to be deployed. The deploy then carries BOTH
# channels (the freshly generated one + the overlaid live one).
#
# It NEVER overwrites a locally-generated file: if the local tree already has a
# manifest at a path (i.e. that IS the channel being published), the local copy
# wins. We only fill in the OTHER channel's gaps.
#
# Usage:
#   scripts/fetch-live-channel.sh <channel> <tree-dir> <updates-base-url>
# e.g.
#   scripts/fetch-live-channel.sh dev updates https://driven.maxhogan.dev/updates
#
# A missing live manifest (HTTP 404 - that platform/channel was never published)
# is skipped, not an error. A network/transport failure is logged and skipped so
# a transient blip never blocks a release deploy (worst case: the other channel's
# manifest is briefly absent until its own workflow re-publishes it; that is
# strictly better than wiping it AND failing the deploy).

set -uo pipefail

CHANNEL="${1:?usage: fetch-live-channel.sh <channel> <tree-dir> <updates-base-url>}"
TREE_DIR="${2:?usage: fetch-live-channel.sh <channel> <tree-dir> <updates-base-url>}"
BASE_URL="${3:?usage: fetch-live-channel.sh <channel> <tree-dir> <updates-base-url>}"

# Strip any trailing slash from the base URL.
BASE_URL="${BASE_URL%/}"

# The GA platform matrix: <os>/<arch> path segments matching the updater
# endpoint layout updates/<channel>/<os>/<arch>/update.json. Keep this in sync
# with the build matrix in release.yml / dev-channel.yml and the targetForBundle
# mapping in generate-update-json.mjs.
PLATFORMS=(
  "windows/x86_64"
  "darwin/x86_64"
  "darwin/aarch64"
  "linux/x86_64"
)

overlaid=0
for plat in "${PLATFORMS[@]}"; do
  rel="${CHANNEL}/${plat}/update.json"
  dest="${TREE_DIR}/${rel}"
  if [ -f "$dest" ]; then
    # Locally generated (this IS the channel being published) - never clobber.
    echo "keep local: ${rel}"
    continue
  fi
  url="${BASE_URL}/${rel}"
  tmp="$(mktemp)"
  code="$(curl -fsSL -o "$tmp" -w '%{http_code}' "$url" || true)"
  if [ "$code" = "200" ] && [ -s "$tmp" ]; then
    mkdir -p "$(dirname "$dest")"
    mv "$tmp" "$dest"
    overlaid=$((overlaid + 1))
    echo "overlaid live: ${rel}"
  else
    rm -f "$tmp"
    echo "skip (http ${code:-error}): ${rel}"
  fi
done

echo "fetch-live-channel: overlaid ${overlaid} live ${CHANNEL} manifest(s) into ${TREE_DIR}"
