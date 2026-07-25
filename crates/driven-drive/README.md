# `driven-drive`

The `RemoteStore` seam plus its implementations. Everything the sync engine knows
about a backup destination goes through this trait, so `driven-core` never names
Google Drive directly.

- `remote_store.rs` - the trait every backend must satisfy (list, create, upload, delete)
- `google/` - the production Google Drive backend: `oauth.rs` (PKCE loopback flow),
  `token_store.rs` (refresh token in the OS keychain), `resumable.rs` (resumable
  uploads), `retry.rs`, `pagination.rs`
- `fake/` - `InMemoryRemoteStore`, the backend behind the contract tests and every
  sync-engine test in the workspace; `fault_injection.rs` makes it fail on demand

Depends only on `driven-tls` (custom CA + proxy config, re-exported here so callers
need no direct dependency). Consumed by `driven-core`, `driven-cli`, `driven-chaos`,
and `src-tauri`. OAuth and the Drive layout are specified in `design/SPEC.md` s4.

```sh
cargo test -p driven-drive              # fake contract suite
just test-e2e-real                      # real Drive, env-gated; see design/E2E_REAL.md
```
