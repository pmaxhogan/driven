# `driven-net`

The production `Backend` behind `driven-core::network`'s `NetworkProbe` seam.
`driven-core` ships only the transport-agnostic probe topology and the per-service
circuit breakers and stays I/O-free; this crate supplies the real clients
(`design/DESIGN.md` s5.8).

- `reachability.rs` - the native per-OS connectivity read (`INetworkListManager` on
  Windows, NetworkManager on Linux, `NWPathMonitor` on macOS) with a dual-stack TCP
  fallback when the native verdict is unavailable or ambiguous
- `classify.rs` - turning a probe response into a `ProbeOutcome` (offline, captive
  portal, DNS failure, service down)
- `lib.rs` - the `Backend` impl: captive-portal probe, per-service health probes with
  per-service timeouts, and connection-pool teardown

Every client is built through `driven_tls::apply_proxy` / `apply_custom_ca`, so the
user's proxy and corporate-CA settings apply here too. Consumed by `driven-cli` and
`src-tauri`; the tests drive the topology through `FakeNetwork` from
`driven-test-fixtures`.

```sh
cargo test -p driven-net
```
