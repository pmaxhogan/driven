# `driven-vss-helper`

The least-privilege VSS elevation helper (`design/DESIGN.md` s5.3.1, issue #25).
Snapshot creation needs Administrator; rather than elevate the whole app with its
OAuth tokens and network stack, only the shadow-copy operation runs elevated, in a
small broker that streams the locked file's bytes back over a secured named pipe.

- `protocol.rs` - the length-prefixed, capped wire framing and control vocabulary
- `validate.rs` - boundary validation of every request; the un-elevated caller is
  untrusted
- `auth.rs` - the pipe security-descriptor SDDL and the caller-identity decision logic
- `launch.rs` - the elevated-launch argv and pipe-name construction
- `provider.rs` - `BrokeredVssProvider`, the app-side `driven_vss::VssProvider` impl
- `server.rs` / `client.rs` / `bin/driven-vss-helper.rs` - the `#[cfg(windows)]` pipe
  server, its client, and the broker binary itself

The decision logic all compiles and unit-tests on every target; only the real pipe,
VSS call, and `ShellExecute runas` are Windows-gated. This is a security boundary -
read the `lib.rs` module doc before changing anything under `auth.rs` or `validate.rs`.

```sh
cargo test -p driven-vss-helper
```
