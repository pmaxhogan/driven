# Real-Drive end-to-end tests (M4)

The `crates/driven-drive/tests/google_e2e.rs` suite runs the SAME portable
`RemoteStore` contract scenarios as `fake_contract.rs`, but against a LIVE
`GoogleDriveStore` built from a real Google refresh token. It exercises the
production OAuth refresh path, the Drive v3 REST surface (multipart create,
resumable upload, PATCH update + appProperties merge, trash, list, metadata,
download, `find_by_op_uuid`, `about`), and the md5-verify-on-upload check.

The suite is GATED on environment variables. When they are absent it prints a
clear `skipping real-Drive e2e (...)` line per test and returns Ok WITHOUT
failing - a credential-less CI run is a no-op pass, not a red build. The tests
are NOT `#[ignore]`d; they are wired and ready and flip on the moment the env
vars are set. This is an honest capability gate, not a faked-green skip.

## Gate variables

| Variable                     | Required | Meaning                                                                 |
|------------------------------|----------|-------------------------------------------------------------------------|
| `DRIVEN_E2E_REFRESH_TOKEN`   | yes      | A Google OAuth refresh token with the `drive` scope for a test account. |
| `DRIVEN_E2E_DEST_FOLDER_ID`  | yes      | The Drive folder id the tests upload under (each test makes a UUID child). |
| `DRIVEN_OAUTH_CLIENT_SECRET` | yes      | The OAuth client secret used to refresh the token (no public default).  |
| `DRIVEN_OAUTH_CLIENT_ID`     | no       | OAuth client id. Defaults to the public installed-app client id.        |

All three required vars must be non-empty; a missing one closes the gate.

Each test:
1. Builds a `RefreshingTokenSource` from the refresh token (the first call
   refreshes the access token), then a `GoogleDriveStore`.
2. Creates a fresh `driven-e2e-<uuid>` child folder under
   `DRIVEN_E2E_DEST_FOLDER_ID` so concurrent runs never collide.
3. Runs its `common::scenario_*` against that child folder.
4. Trashes the child folder on success AND on failure (a scenario panic is
   caught, the folder is trashed, then the panic is re-raised so the test
   still fails). Trashing the folder removes its whole subtree.

## Minting the token (one-time, on a machine with a browser)

The `driven-cli` debug tool runs the SPEC s4 PKCE loopback OAuth flow and
stores the resulting refresh token in the OS keychain, then prints it.

1. Put the Google "installed app" OAuth client config at the repo root as
   `client_secret.json` (it is gitignored - matched by `client_secret*.json`).
   It is the standard console download:
   `{"installed": {"client_id": "...", "client_secret": "...", ...}}`.
   Alternatively pass `--client-id` / `--client-secret` (or set
   `DRIVEN_OAUTH_CLIENT_ID` / `DRIVEN_OAUTH_CLIENT_SECRET`).

2. Run the auth flow, picking an account label (the keychain "username"):

   ```sh
   cargo run --bin driven-cli -- auth --account e2e
   ```

   This opens your browser to the Google consent screen, captures the
   authorization code on a loopback `127.0.0.1:<port>` listener (validating the
   CSRF state constant-time and the Host header against the exact registered
   authority), exchanges it for tokens, and stores the refresh token in the OS
   keychain under the `e2e` account.

3. Print the stored refresh token:

   ```sh
   cargo run --bin driven-cli -- dump-refresh-token --account e2e
   ```

   The bare token prints to stdout so it can be captured directly.

4. (Optional) Smoke-test an upload against a real Drive folder:

   ```sh
   cargo run --bin driven-cli -- sync \
     --account e2e \
     --source ./some-test-folder \
     --dest-folder-id <DRIVE_FOLDER_ID>
   ```

   `sync` walks the folder's top-level files and creates each on Drive
   (updating by id if a same-named file already exists), printing the resulting
   id, size, and md5. This is the ROADMAP M4 acceptance "upload a 3-file test
   folder" path.

## Running the e2e suite locally

Set the gate and run only the e2e test target:

```sh
export DRIVEN_E2E_REFRESH_TOKEN="$(cargo run --bin driven-cli -- dump-refresh-token --account e2e)"
export DRIVEN_E2E_DEST_FOLDER_ID="<DRIVE_FOLDER_ID>"
export DRIVEN_OAUTH_CLIENT_SECRET="<from client_secret.json>"
# DRIVEN_OAUTH_CLIENT_ID defaults to the public installed-app id; override if needed.

cargo test -p driven-drive --test google_e2e -- --nocapture
```

With the gate unset, the same command prints the skip lines and passes:

```sh
cargo test -p driven-drive --test google_e2e
# skipping real-Drive e2e (google_round_trip): set DRIVEN_E2E_REFRESH_TOKEN + ...
```

You can also persist the vars in a local `.env.test` (gitignored) and source it
before the run, rather than exporting each time:

```sh
# .env.test  (DO NOT COMMIT - add to .gitignore if not already covered)
DRIVEN_E2E_REFRESH_TOKEN=...
DRIVEN_E2E_DEST_FOLDER_ID=...
DRIVEN_OAUTH_CLIENT_SECRET=...
```

```sh
set -a; . ./.env.test; set +a
cargo test -p driven-drive --test google_e2e -- --nocapture
```

## CI: the chaos-real-drive job

The real-Drive e2e job in CI runs the SAME `google_e2e` target with the gate
supplied from GitHub Actions secrets. Because the suite no-op-passes without the
gate, it is safe to run on every relevant build; it only does real Drive I/O
when the secrets are configured.

Configure these repository (or environment) secrets in GitHub:

- `DRIVEN_E2E_REFRESH_TOKEN`
- `DRIVEN_E2E_DEST_FOLDER_ID`
- `DRIVEN_OAUTH_CLIENT_SECRET`
- (optional) `DRIVEN_OAUTH_CLIENT_ID`

The job step maps the secrets into the process env and runs the target:

```yaml
  chaos-real-drive:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - name: Real-Drive e2e (gate flips on only when secrets are set)
        env:
          DRIVEN_E2E_REFRESH_TOKEN: ${{ secrets.DRIVEN_E2E_REFRESH_TOKEN }}
          DRIVEN_E2E_DEST_FOLDER_ID: ${{ secrets.DRIVEN_E2E_DEST_FOLDER_ID }}
          DRIVEN_OAUTH_CLIENT_SECRET: ${{ secrets.DRIVEN_OAUTH_CLIENT_SECRET }}
          DRIVEN_OAUTH_CLIENT_ID: ${{ secrets.DRIVEN_OAUTH_CLIENT_ID }}
        run: cargo test -p driven-drive --test google_e2e -- --nocapture
```

How the gate "flips on": when a fork PR or a credential-less branch builds, the
secrets resolve to empty strings, `e2e_creds` closes the gate, and every test
prints its skip line and passes. When the secrets are present (a trusted branch
or a maintainer's local run), the same target builds a live store and runs the
full contract against real Drive, cleaning up its per-test child folders.

### Token maintenance

Google refresh tokens for a Testing-status OAuth app expire after 7 days; a
Published app's refresh tokens are long-lived. If the e2e job starts failing
with `auth.invalid_grant`, the refresh token was revoked or expired - re-mint it
via the `driven-cli auth` -> `dump-refresh-token` flow above and update the
`DRIVEN_E2E_REFRESH_TOKEN` secret. Use a dedicated throwaway test Google account
with a single dedicated `DRIVEN_E2E_DEST_FOLDER_ID` folder, never a real account.
