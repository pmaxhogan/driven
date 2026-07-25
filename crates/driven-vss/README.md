# `driven-vss`

Windows Volume Shadow Copy Service reads, so files held under an exclusive write
lock (Outlook PSTs, running database files, hypervisor disks) still get backed up
instead of being skipped forever (`design/DESIGN.md` s5.3).

- `mode.rs` - the persisted `auto` / `always` / `never` setting (`windows.vss_mode`)
- `provider.rs` - the `VssProvider` seam the executor's open path consults, plus
  `FakeVssProvider` for the cross-OS tests
- `lib.rs` - `is_elevated` and `fallback_decision`, the pure "open failed, now what"
  function that is table-tested on every OS
- `orphan.rs` - the snapshot-ownership ledger and the >1h prune that releases shadow
  copies an unclean shutdown left behind
- `windows_vss.rs` - the real `IVssBackupComponents` COM sequence, `#[cfg(windows)]`
  only; `stub.rs` returns `VssError::Unavailable` elsewhere so the degrade path is
  identical to an un-elevated Windows host

The COM interface is hand-declared because win32metadata never projected it - see
`design/CODEX_NOTES.md`. Snapshot creation needs Administrator, which is why
`driven-vss-helper` exists. Consumed by `driven-core`, `driven-chaos`, `src-tauri`.

```sh
cargo test -p driven-vss
```
