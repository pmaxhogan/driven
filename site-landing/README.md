# `site-landing/`

The marketing landing page served at the root of `driven.maxhogan.dev`. Hand-written
static files, no build step and no framework - open `index.html` in a browser to
preview.

- `index.html` - the page itself
- `styles.css` - all of its styling
- `icon.svg` - the Driven mark
- `404.html` - the branded 404 for the whole site

These four files are the only ones published: `scripts/assemble-landing.sh` copies
them by name into the deploy staging root, so anything else added here stays local.

The site shares one Cloudflare Pages project (`driven-updates`) with the updater
manifests under `/updates`, and a Pages deploy replaces the whole site. That is why
`.github/workflows/deploy-landing.yml` reassembles the landing *and* overlays both
channels' live manifests before publishing - never deploy the landing on its own.
Any push to `main` touching this directory triggers that workflow.
