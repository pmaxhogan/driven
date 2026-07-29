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

| Variable                      | Required | Meaning                                                                 |
|-------------------------------|----------|-------------------------------------------------------------------------|
| `DRIVEN_E2E_REFRESH_TOKEN`    | yes      | A Google OAuth refresh token with the `drive` scope for a test account. |
| `DRIVEN_E2E_DEST_FOLDER_ID`   | yes      | The Drive folder id the tests upload under (each test makes a UUID child). |
| `DRIVEN_OAUTH_CLIENT_SECRET`  | yes      | The OAuth client secret used to refresh the token (no public default).  |
| `DRIVEN_OAUTH_CLIENT_ID`      | no       | OAuth client id. Defaults to the public installed-app client id.        |
| `DRIVEN_E2E_SHARED_DRIVE_ID`  | no       | A Google **Shared Drive** id (issue #7). Opens a SECOND, independent gate covering only the 5 `google_shared_drive_*` tests. |

All three required vars must be non-empty; a missing one closes the gate.

### The gate has two independent tiers

This is the part that surprises people, so it is worth stating plainly: setting
the three required vars above does **not** make all 13 tests do real Drive I/O.
There are two separate gates, each with its own skip message.

| Tier | Gated on | Tests | Skip message |
|------|----------|-------|--------------|
| Base | `DRIVEN_E2E_REFRESH_TOKEN` + `DRIVEN_E2E_DEST_FOLDER_ID` + `DRIVEN_OAUTH_CLIENT_SECRET` | the 8 non-Shared-Drive tests | `skipping real-Drive e2e (<test>): ...` |
| Shared Drive | the base tier **plus** `DRIVEN_E2E_SHARED_DRIVE_ID` | the 5 `google_shared_drive_*` tests | `skipping Shared Drive e2e (<test>): ...` |

So a fully credentialed run with no `DRIVEN_E2E_SHARED_DRIVE_ID` reports
**13 passed** with 5 `skipping Shared Drive e2e` lines still present. That is the
expected, healthy outcome - not a regression, and not a sign the creds are
wrong. Only the disappearance of the *base* tier's `skipping real-Drive e2e`
lines tells you the credentials took effect.

The Shared Drive tests exist because a Shared Drive is a genuinely different
Drive backend: its id doubles as its root folder id, and every listing must be
scoped to that drive or the store list-empties and re-uploads (issue #7). Each
one creates its UUID-named child folder directly under the Shared Drive root and
drives the portable scenarios with a `SharedDrive` context instead of a My Drive
one.

**A consumer Google account cannot satisfy this tier.** Shared Drives are a
Google Workspace feature; a plain `@gmail.com` account has none, so there is no
id to put in the variable. `GET https://www.googleapis.com/drive/v3/drives`
authenticated as such an account returns an empty `drives` list (a successful
call, not a permission error) - which is exactly what the dedicated e2e account
returns today. Running this tier against real Drive therefore requires a
Workspace account; until one is wired up, the Shared Drive contract is covered
only by the fake-store tests in `fake_contract.rs`. The CI `chaos-real-drive`
job likewise sets no `DRIVEN_E2E_SHARED_DRIVE_ID`, so these 5 tests skip there
too.

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
# DRIVEN_E2E_SHARED_DRIVE_ID is optional and needs a Workspace account; without
# it the 5 google_shared_drive_* tests skip. See "The gate has two independent
# tiers" above.

cargo test -p driven-drive --test google_e2e -- --nocapture
```

A credentialed run without `DRIVEN_E2E_SHARED_DRIVE_ID` looks like this - 8
tests doing real Drive I/O, 5 honestly skipping, all 13 green:

```
skipping Shared Drive e2e (google_shared_drive_round_trip): set DRIVEN_E2E_SHARED_DRIVE_ID to run
...
test result: ok. 13 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

With the gate unset, the same command prints the base skip lines and passes:

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
# Optional, Workspace-only; omit to skip the 5 google_shared_drive_* tests.
DRIVEN_E2E_SHARED_DRIVE_ID=...
```

```sh
set -a; . ./.env.test; set +a
cargo test -p driven-drive --test google_e2e -- --nocapture
```

## CI: the chaos-real-drive job

The real-Drive e2e job (`chaos-real-drive` in `.github/workflows/chaos.yml`) runs
the SAME `google_e2e` target with the gate supplied from GitHub Actions secrets.
Because the suite no-op-passes without the gate, it is safe anywhere; it only does
real Drive I/O when the secrets are configured. Per the chaos COST POLICY it is
gated to `v*` TAG pushes only (`if: startsWith(github.ref, 'refs/tags/')`) - real
Google traffic on every push/PR is too costly - so real-Drive coverage runs at
release time, not per-PR. The job has the display name `real-drive e2e
(tag-only)`. The block below mirrors the live job verbatim (kept in sync with
`.github/workflows/chaos.yml`).

Configure these repository (or environment) secrets in GitHub:

- `DRIVEN_E2E_REFRESH_TOKEN`
- `DRIVEN_E2E_DEST_FOLDER_ID`
- `DRIVEN_OAUTH_CLIENT_SECRET`
- (optional) `DRIVEN_OAUTH_CLIENT_ID`

The job maps the secrets into the process env at the JOB level (so every step
sees them) and runs the target. It also installs `libssl-dev` for the Drive HTTP
client and restores the shared `workspace` rust-cache:

```yaml
  chaos-real-drive:
    name: real-drive e2e (tag-only)
    if: ${{ startsWith(github.ref, 'refs/tags/') }}
    runs-on: ubuntu-latest
    env:
      DRIVEN_E2E_REFRESH_TOKEN: ${{ secrets.DRIVEN_E2E_REFRESH_TOKEN }}
      DRIVEN_E2E_DEST_FOLDER_ID: ${{ secrets.DRIVEN_E2E_DEST_FOLDER_ID }}
      DRIVEN_OAUTH_CLIENT_SECRET: ${{ secrets.DRIVEN_OAUTH_CLIENT_SECRET }}
      DRIVEN_OAUTH_CLIENT_ID: ${{ secrets.DRIVEN_OAUTH_CLIENT_ID }}
    steps:
      - uses: actions/checkout@v7
      - uses: dtolnay/rust-toolchain@stable
      - name: Install Linux build deps (libssl for the Drive HTTP client)
        run: |
          sudo apt-get update
          sudo apt-get install -y libssl-dev
      - uses: Swatinem/rust-cache@v2
        with:
          shared-key: "workspace"
          save-if: ${{ github.ref == 'refs/heads/main' }}
      - name: real-drive e2e (google_e2e)
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
