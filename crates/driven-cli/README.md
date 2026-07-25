# `driven-cli`

A small headless debugging CLI: do real Drive work, and inspect the local state
database, without launching the GUI. Not shipped to users.

- `main.rs` - the clap command tree: `auth` (PKCE loopback flow, stores the refresh
  token in the OS keychain), `dump-refresh-token` and `dump-client-creds` (for
  debugging refresh failures and minting `DRIVEN_E2E_REFRESH_TOKEN`), and `sync`
  (one sync cycle of a local folder to a real Drive folder)
- `inspect.rs` - the read-only state-database commands: `status`, `history`, `verify`
  (exits non-zero when any file is in a corrupt / error state); no network access

Uses only the public `driven-drive` surface plus clap / tokio / anyhow / tracing - it
has no HTTP or serde dependency of its own, deliberately. OAuth client credentials
come from a gitignored `client_secret.json` at the repo root, `--client-id` /
`--client-secret`, or the public installed-app default. See `design/SPEC.md` s4.

```sh
cargo run --bin driven-cli -- auth --account me
cargo test -p driven-cli
```
