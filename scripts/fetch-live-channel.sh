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
#   scripts/fetch-live-channel.sh dev site/updates https://driven.maxhogan.dev/updates
#
# R7-P1-1: <tree-dir> is the local `updates/` tree that the workflow will deploy.
# The workflows assemble it under a `site/` staging parent (so it is
# `site/updates/<channel>/...`) and `wrangler pages deploy site` - deploying the
# PARENT keeps the served `/updates/` URL prefix that the in-app updater fetches.
# This script appends `<channel>/<plat>/update.json` to whatever <tree-dir> it is
# given, so it works unchanged whether the tree root is `updates` or `site/updates`.
#
# FAIL-CLOSED policy (R4-P1-4). The deploy that follows is a WHOLE-SITE snapshot,
# so any OTHER-channel manifest we fail to overlay here is WIPED off the live site
# by the deploy - breaking auto-update for every user on that channel. Treating a
# transport / 5xx / non-200 fetch as "skip" therefore silently destroys the other
# channel on a transient blip. So:
#   - A genuine HTTP 404 is the ONLY tolerated miss: it means that platform on the
#     OTHER channel was never published yet (the known first-publish case), so
#     there is nothing to preserve.
#   - ANY other failure (transport error, timeout, 5xx, 403, a 200 with an empty
#     body, etc.) is retried a few times; if it still fails, the script EXITS
#     NON-ZERO so the calling workflow ABORTS the deploy. A briefly-skipped deploy
#     is strictly better than wiping a live channel's manifests.
#
# A durable source of truth (e.g. committing each channel's published manifests to
# the repo, or keeping a canonical R2/KV copy and deploying the MERGE of both
# channels from there) would remove this fetch dependency entirely; see
# design/CODEX_NOTES.md "## M9 fix round 4b". Until then, fail closed.

set -uo pipefail

# Per-URL fetch tuning: a few retries with backoff to ride out a transient blip,
# then fail closed. curl's own retry covers transient transport/5xx; we also wrap
# it in an outer loop so a non-retryable-by-curl miss (e.g. a flaky 200/empty) is
# retried too.
FETCH_ATTEMPTS="${FETCH_LIVE_ATTEMPTS:-4}"
FETCH_RETRY_DELAY="${FETCH_LIVE_RETRY_DELAY:-3}"

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

# Fetch one URL into $tmp with retries. Echoes a result token on stdout:
#   ok    - a 200 with a non-empty body (tmp holds the manifest)
#   404   - a genuine 404 (the OTHER channel never published this platform)
#   fail  - persistent non-404 failure after all retries (caller must fail closed)
# Diagnostics go to stderr so they do not pollute the result token.
fetch_one() {
  url="$1"
  out="$2"
  attempt=1
  while [ "$attempt" -le "$FETCH_ATTEMPTS" ]; do
    # -w writes ONLY the http_code to stdout; transport failures make curl exit
    # non-zero and may emit no code, so default to 000. --retry handles curl's
    # own transient 5xx/transport retries within the attempt.
    code="$(curl -sSL --retry 3 --retry-delay 2 --retry-all-errors \
      --connect-timeout 15 --max-time 120 \
      -o "$out" -w '%{http_code}' "$url" 2>/dev/null || true)"
    code="${code:-000}"
    if [ "$code" = "200" ] && [ -s "$out" ]; then
      echo "ok"
      return 0
    fi
    if [ "$code" = "404" ]; then
      # A definitive "not published yet" - do NOT retry, do NOT fail.
      echo "404"
      return 0
    fi
    echo "fetch attempt ${attempt}/${FETCH_ATTEMPTS} for ${url} failed (http ${code})" >&2
    rm -f "$out"
    if [ "$attempt" -lt "$FETCH_ATTEMPTS" ]; then
      sleep "$FETCH_RETRY_DELAY"
    fi
    attempt=$((attempt + 1))
  done
  echo "fail"
  return 0
}

overlaid=0
failed=0
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
  result="$(fetch_one "$url" "$tmp")"
  case "$result" in
    ok)
      mkdir -p "$(dirname "$dest")"
      mv "$tmp" "$dest"
      overlaid=$((overlaid + 1))
      echo "overlaid live: ${rel}"
      ;;
    404)
      rm -f "$tmp"
      echo "first-publish (404, nothing to preserve): ${rel}"
      ;;
    *)
      # Persistent NON-404 failure. FAIL CLOSED: record it and keep going so the
      # log lists EVERY failing manifest, then exit non-zero below to ABORT the
      # deploy. Deploying now would wipe this live ${CHANNEL} manifest.
      rm -f "$tmp"
      echo "::error::fetch-live-channel: persistent fetch failure for ${url}; refusing to deploy a partial site that would wipe the live ${CHANNEL} channel"
      failed=$((failed + 1))
      ;;
  esac
done

if [ "$failed" -gt 0 ]; then
  echo "fetch-live-channel: FAILED CLOSED - ${failed} live ${CHANNEL} manifest(s) could not be preserved; aborting before the whole-site deploy" >&2
  exit 1
fi

echo "fetch-live-channel: overlaid ${overlaid} live ${CHANNEL} manifest(s) into ${TREE_DIR}"
