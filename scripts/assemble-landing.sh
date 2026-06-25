#!/usr/bin/env bash
# assemble-landing.sh (M12).
#
# Copies the committed root landing page (site-landing/) into the whole-site
# deploy staging root (site/). `wrangler pages deploy site` publishes a
# WHOLE-SITE snapshot to the driven-updates CF Pages project (which serves
# driven.maxhogan.dev), so EVERY deploy must include BOTH the landing page AND
# both channels' live updates/ manifests, or whatever is missing gets wiped.
#
# This script only handles the LANDING half: it lays down index.html, styles.css,
# icon.svg, and 404.html at the ROOT of site/ (so `/` serves the landing and `/`
# 404s render the branded 404). The updates/ tree is assembled separately by the
# calling workflow (the channel manifest generators + scripts/fetch-live-channel.sh
# overlay), and this copy is ADDITIVE - it never touches site/updates/.
#
# Usage:
#   scripts/assemble-landing.sh [site-dir] [landing-dir]
# Defaults: site-dir=site, landing-dir=site-landing
#
# Idempotent: safe to run before or after the updates/ tree is assembled, and
# safe to re-run. It does NOT delete anything under site/.

set -euo pipefail

SITE_DIR="${1:-site}"
LANDING_DIR="${2:-site-landing}"

if [ ! -d "$LANDING_DIR" ]; then
  echo "::error::assemble-landing: landing dir '${LANDING_DIR}' not found" >&2
  exit 1
fi

mkdir -p "$SITE_DIR"

# Copy the landing files to the site root. We copy the known landing assets
# explicitly (not the dir wholesale) so a stray file under site-landing/ cannot
# accidentally land at the site root. The icon is the C2 road-to-cloud master.
copied=0
for f in index.html styles.css icon.svg 404.html; do
  src="${LANDING_DIR}/${f}"
  if [ -f "$src" ]; then
    cp "$src" "${SITE_DIR}/${f}"
    copied=$((copied + 1))
    echo "landing: copied ${f} -> ${SITE_DIR}/${f}"
  fi
done

# index.html, styles.css, and icon.svg are required for a usable landing; 404.html
# is optional. Fail closed if a required file is missing so we never deploy a
# half-built landing over the live one.
for required in index.html styles.css icon.svg; do
  if [ ! -f "${SITE_DIR}/${required}" ]; then
    echo "::error::assemble-landing: required landing file '${required}' missing after copy" >&2
    exit 1
  fi
done

echo "assemble-landing: copied ${copied} landing file(s) into ${SITE_DIR}/"
