# `driven-tls`

The one shared home for custom-root-CA and proxy support. Every outbound `reqwest`
client in the workspace routes its builder through here, so a user-configured
corporate / TLS-inspection CA and proxy apply to all connections
(`design/DESIGN.md` s5.8.7).

- `lib.rs` - `apply_custom_ca` / `load_certificates` and the `CustomCaConfig` type
- `proxy.rs` - `ProxyConfig` and `apply_proxy`, covering system, manual (incl. SOCKS5),
  PAC, and no-proxy modes; PAC files are evaluated with the embedded `boa_engine` JS
  runtime because no maintained PAC crate exists

A leaf crate: it depends only on `reqwest` + `thiserror`, which is what lets it sit
below `driven-net`, `driven-drive`, and `src-tauri` without a cycle. The trust rules
in the `lib.rs` module doc are load-bearing and were reviewed line by line - CA
configuration is strictly additive, never bypasses verification, and fails closed.

```sh
cargo test -p driven-tls
```
