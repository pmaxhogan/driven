# Driven — Technical Specification

> Companion to `DESIGN.md`. Read that first.
>
> This document is concrete enough that an autonomous agent can implement
> the whole project from it without further user input. Anything ambiguous
> here is a bug — file an issue and we'll tighten it.

---

## 0. Conventions

- Rust edition 2021. MSRV: pinned in workspace `Cargo.toml`'s `rust-version`
  to the version CI tests against; CI runs both that pinned version AND
  latest stable. Bumping MSRV is allowed in any release; it is not a
  semver-affecting promise.
- All async code uses `tokio` 1.x. Avoid `async-std`.
- All errors flow through `anyhow::Result` at app boundaries and
  `thiserror`-derived enums inside library crates. No `unwrap()` /
  `expect()` outside of tests and trivially-unreachable invariants.
- Logging via `tracing`. Each crate has a module-level
  `static TARGET: &str = "driven::core::scanner";` etc.
- No `println!` in library code; the Tauri shell may println at startup.
- Frontend: TypeScript strict mode on. ESLint + Prettier; double quotes,
  semis, 2-space indent (matches user's global preference in CLAUDE.md).
- Comments only when *why* is non-obvious. No file headers. No
  block comments restating what code does.
- Commit messages: Conventional Commits (`feat:`, `fix:`, `chore:` …).
  release-please depends on this.
- Git identity per the user's global rules: `user.email = max@maxhogan.dev`,
  `user.name = pmaxhogan`.
- ASCII dashes everywhere. No em/en-dashes per global CLAUDE.md.
- LF line endings; `.gitattributes` must set `* text=auto eol=lf`.

---

## 1. Repository layout

```
driven/
├─ Cargo.toml                          # workspace root
├─ Cargo.lock
├─ rust-toolchain.toml                 # pin "stable", clippy/rustfmt
├─ .gitattributes                      # * text=auto eol=lf
├─ .gitignore
├─ .editorconfig
├─ rustfmt.toml
├─ deny.toml                           # cargo-deny config
├─ release-please-config.json
├─ .release-please-manifest.json
├─ justfile                            # dev recipes
├─ README.md
├─ CHANGELOG.md                        # managed by release-please
├─ LICENSE                             # MIT or Apache-2.0; user to pick
│
├─ design/
│  ├─ DESIGN.md                        # the why
│  ├─ SPEC.md                          # this file
│  ├─ ROADMAP.md                       # phased milestones
│  ├─ THREATS.md                       # threat model
│  ├─ CRYPTO_FORMAT.md                 # encrypted-format spec
│  └─ STRESS_HARNESS.md                # stress / chaos test harness spec (sister project)
│
├─ crates/
│  ├─ driven-core/
│  │  ├─ Cargo.toml
│  │  └─ src/
│  │     ├─ lib.rs
│  │     ├─ state.rs                   # SQLite repo wrappers
│  │     ├─ migrations/                # *.sql migration files (sqlx)
│  │     ├─ scanner.rs                 # local walk
│  │     ├─ planner.rs                 # diff → ops
│  │     ├─ orchestrator.rs            # state machine
│  │     ├─ pacer.rs                   # per-account token bucket
│  │     ├─ scheduler.rs               # tick + cron-window
│  │     ├─ activity.rs                # activity log writer
│  │     ├─ exclude.rs                 # include/exclude pattern eval
│  │     ├─ pending_ops.rs             # work queue
│  │     ├─ verify.rs                  # deep-verify cycle
│  │     ├─ watcher.rs                 # notify-crate fs watcher per source
│  │     ├─ types.rs                   # shared types (Op, Entry, Plan …)
│  │     ├─ time.rs                    # Clock trait + impls
│  │     └─ tests/
│  │        ├─ scanner.rs
│  │        ├─ planner.rs
│  │        ├─ orchestrator_snapshots.rs
│  │        └─ ...
│  │
│  ├─ driven-drive/
│  │  ├─ Cargo.toml
│  │  └─ src/
│  │     ├─ lib.rs
│  │     ├─ remote_store.rs            # the trait
│  │     ├─ google/
│  │     │  ├─ mod.rs                  # GoogleDriveStore impl
│  │     │  ├─ oauth.rs                # PKCE loopback flow
│  │     │  ├─ token_store.rs          # keyring-backed refresh storage
│  │     │  ├─ pagination.rs
│  │     │  ├─ resumable.rs
│  │     │  └─ retry.rs                # exponential backoff
│  │     ├─ fake/
│  │     │  └─ mod.rs                  # InMemoryRemoteStore
│  │     └─ tests/
│  │        ├─ fake_contract.rs        # contract tests both impls must pass
│  │        └─ google_e2e.rs           # gated on env var
│  │
│  ├─ driven-crypto/
│  │  ├─ Cargo.toml
│  │  └─ src/
│  │     ├─ lib.rs
│  │     ├─ key.rs                     # master/per-source key types
│  │     ├─ keystore.rs                # keyring crate wrapper
│  │     ├─ filename.rs                # path-component AEAD + base32
│  │     ├─ content.rs                 # chunked stream cipher
│  │     ├─ recovery.rs                # BIP39 over master key
│  │     └─ tests/
│  │
│  ├─ driven-power/
│  │  ├─ Cargo.toml
│  │  └─ src/
│  │     ├─ lib.rs                     # PowerSource trait
│  │     ├─ windows.rs                 # GetSystemPowerStatus + WM_POWER
│  │     ├─ macos.rs                   # IOPSCopyPowerSourcesInfo
│  │     ├─ linux.rs                   # /sys/class/power_supply
│  │     ├─ network.rs                 # metered detection
│  │     └─ fake.rs                    # in-test impl
│  │
│  ├─ driven-test-fixtures/
│  │  ├─ Cargo.toml                    # publish=false
│  │  └─ src/
│  │     ├─ lib.rs
│  │     ├─ tree.rs                    # tree!() macro
│  │     ├─ clock.rs                   # FakeClock
│  │     └─ remote.rs                  # remote-store assertions
│  │
│  └─ driven-chaos/                    # stress/chaos harness; see design/STRESS_HARNESS.md
│     ├─ Cargo.toml                    # publish=false; binary crate
│     └─ src/
│        ├─ main.rs                    # subcommand dispatch
│        ├─ scenario.rs                # Scenario trait + Verdict + Outcome
│        ├─ handle.rs                  # DrivenHandle: hermetic core boot
│        ├─ capabilities.rs            # CapabilitySet probe
│        ├─ mutator.rs                 # filesystem + drive-side mutators
│        ├─ fuzz.rs                    # seeded fuzz driver
│        ├─ report.rs                  # JSON + human report writers
│        └─ scenarios/                 # one module per category in STRESS_HARNESS.md §3
│           ├─ mod.rs
│           ├─ storage.rs
│           ├─ size_extremes.rs
│           ├─ permissions.rs
│           ├─ filenames.rs
│           ├─ ntfs_hazards.rs
│           ├─ mutation.rs
│           ├─ drive_fuckery.rs
│           └─ concurrency.rs
│
├─ src-tauri/                          # Tauri app
│  ├─ Cargo.toml
│  ├─ tauri.conf.json
│  ├─ build.rs
│  ├─ icons/
│  │  └─ ...
│  └─ src/
│     ├─ main.rs
│     ├─ app_state.rs                  # shared Arc<AppState>
│     ├─ commands/
│     │  ├─ mod.rs
│     │  ├─ accounts.rs
│     │  ├─ sources.rs
│     │  ├─ sync.rs
│     │  ├─ activity.rs
│     │  ├─ restore.rs
│     │  ├─ settings.rs
│     │  └─ diagnostics.rs
│     ├─ events.rs                     # outbound IPC events
│     ├─ tray.rs
│     ├─ updater.rs
│     ├─ telemetry.rs
│     ├─ panic_hook.rs
│     ├─ i18n.rs                       # rust-i18n loader for tray/notifs
│     └─ migrations.rs                 # runs sqlx migrations on boot
│  └─ locales/                         # Rust-side locale files (tray, OS notifs)
│     └─ en-US.yml
│
├─ ui/                                 # Vue 3 frontend
│  ├─ package.json
│  ├─ pnpm-lock.yaml
│  ├─ vite.config.ts
│  ├─ tsconfig.json
│  ├─ tailwind.config.ts
│  ├─ index.html
│  └─ src/
│     ├─ main.ts
│     ├─ App.vue
│     ├─ router.ts
│     ├─ ipc/                          # typed wrappers over @tauri-apps/api
│     │  ├─ accounts.ts
│     │  ├─ sources.ts
│     │  └─ ...
│     ├─ stores/
│     │  ├─ accounts.ts
│     │  ├─ sources.ts
│     │  ├─ activity.ts
│     │  └─ settings.ts
│     ├─ locales/                      # vue-i18n message bundles
│     │  └─ en-US.json
│     ├─ i18n.ts                       # vue-i18n setup, ICU loader
│     ├─ formatters.ts                 # Intl.* helpers (bytes, dates, numbers)
│     ├─ views/
│     │  ├─ SetupWizard.vue
│     │  ├─ Settings.vue
│     │  ├─ Activity.vue
│     │  ├─ Restore.vue
│     │  └─ About.vue
│     ├─ components/
│     │  ├─ TrayPanel.vue
│     │  ├─ SourceTable.vue
│     │  ├─ AccountList.vue
│     │  ├─ AddSourceWizard.vue
│     │  ├─ ChangelogModal.vue
│     │  └─ RestoreTree.vue
│     └─ utils/
│        └─ format.ts
│
├─ .github/
│  ├─ workflows/
│  │  ├─ ci.yml
│  │  ├─ release-please.yml
│  │  ├─ release.yml                   # tag-triggered, stable channel
│  │  └─ dev-channel.yml               # gated (dispatch OR [dev-build] marker), dev channel
│  └─ release-please-config.json
│
├─ updater/                            # static endpoint payload (also served from gh-pages branch / R2)
│  ├─ stable/update.json
│  └─ dev/update.json
│
└─ tests/                              # workspace-level integration tests
   ├─ e2e_fake.rs                      # full app, in-memory remote
   └─ e2e_real.rs                      # gated on DRIVEN_E2E_REFRESH_TOKEN
```

---

## 2. SQLite schema

Stored at `<config_dir>/driven/state.db`. WAL mode enabled on first open.

Migrations live under `crates/driven-core/src/migrations/` as
`NNNN_<name>.sql`. `sqlx::migrate!()` runs them at boot.

```sql
-- 0001_initial.sql

CREATE TABLE accounts (
  id TEXT PRIMARY KEY,                -- uuid
  email TEXT NOT NULL,
  display_name TEXT,
  state TEXT NOT NULL,                -- 'ok' | 'needs_reauth' | 'disabled'
  encryption_master_key_id TEXT,      -- keychain handle (the key itself isn't stored here)
  created_at INTEGER NOT NULL,
  last_synced_at INTEGER
);

CREATE TABLE backup_sources (
  id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  display_name TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  local_path TEXT NOT NULL,
  drive_folder_id TEXT NOT NULL,
  drive_folder_path TEXT NOT NULL,    -- cached display path
  encryption_enabled INTEGER NOT NULL DEFAULT 0,
  wrapped_source_key BLOB,            -- per-source key, encrypted by master key
  respect_gitignore INTEGER NOT NULL DEFAULT 1,
  include_patterns TEXT NOT NULL DEFAULT '[]',  -- JSON array of globs
  exclude_patterns TEXT NOT NULL DEFAULT '[]',
  schedule_json_v2_reserved TEXT,     -- V2: { windows: [...], min_interval_secs } — NULL in V1, never read by V1 code
  deep_verify_interval_secs INTEGER NOT NULL DEFAULT 604800,
  last_full_scan_at INTEGER,
  last_deep_verify_at INTEGER,
  created_at INTEGER NOT NULL
);

CREATE TABLE file_state (
  source_id TEXT NOT NULL REFERENCES backup_sources(id) ON DELETE CASCADE,
  relative_path TEXT NOT NULL,
  size INTEGER NOT NULL,
  mtime_ns INTEGER NOT NULL,
  hash_blake3 BLOB NOT NULL,          -- 32 bytes, plaintext for encrypted sources
  drive_file_id TEXT,                 -- null until first upload
  drive_md5 BLOB,                     -- 16 bytes; ciphertext md5 if encrypted
  encrypted_remote_path TEXT,         -- cached, for encrypted sources
  status TEXT NOT NULL,               -- 'synced' | 'pending' | 'corrupt' | 'locked' | 'error' | 'excluded_orphan' (DESIGN s5.5: backed-up file now ignored, not trashed)
  last_uploaded_at INTEGER,
  last_verified_at INTEGER,
  PRIMARY KEY (source_id, relative_path)
);
CREATE INDEX idx_file_state_status ON file_state(source_id, status);

CREATE VIRTUAL TABLE file_state_fts USING fts5(
  relative_path,
  content='file_state',
  content_rowid='rowid',
  tokenize='unicode61 remove_diacritics 2'
);
-- triggers keep FTS in sync (omitted here for brevity; see migration file)

CREATE TABLE pending_ops (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  source_id TEXT NOT NULL REFERENCES backup_sources(id) ON DELETE CASCADE,
  op_type TEXT NOT NULL,              -- 'upload' | 'trash' | 'resume' | 'verify'
  relative_path TEXT NOT NULL,
  payload_json TEXT NOT NULL,         -- op-specific payload (resumable session url etc.)
  attempts INTEGER NOT NULL DEFAULT 0,
  last_error TEXT,
  scheduled_for INTEGER NOT NULL,     -- unix epoch ms
  created_at INTEGER NOT NULL
);
CREATE INDEX idx_pending_ops_due ON pending_ops(scheduled_for, source_id);

CREATE TABLE activity_log (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts INTEGER NOT NULL,
  source_id TEXT REFERENCES backup_sources(id) ON DELETE SET NULL,
  level TEXT NOT NULL,                -- 'info' | 'warn' | 'error'
  event_type TEXT NOT NULL,           -- 'scan_done' | 'upload_done' | 'trash_done' | 'paused' | 'error' | ...
  file_count INTEGER,
  bytes INTEGER,
  message TEXT
);
CREATE INDEX idx_activity_ts ON activity_log(ts DESC);

CREATE TABLE settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL                 -- JSON
);
```

Global defaults seeded on first boot via `0002_seed_settings.sql`.

---

## 3. The `RemoteStore` trait

Key Drive semantics this trait must accommodate:

- **Drive allows duplicate names** within the same folder. Every lookup
  is by file_id, never by name alone.
- **Updating an existing object is its own API path** (`PATCH /files/{id}`
  for metadata, `PATCH /upload/files/{id}` for content). `POST` always
  *creates* a new object — using it to "overwrite" yields duplicates.
- **Resumable uploads have a 256 KiB chunk-size constraint** for non-final
  chunks. Final chunks may be any size. (The HTTP chunk size is
  independent of any on-disk buffer size.)
- **`appProperties`** is a 124-key/30-KB-value private metadata map on
  every Drive file. We use it as the **canonical identity** for files
  Driven owns (`driven.source_id`, `driven.relative_path_hash`,
  `driven.client_op_uuid`) — survives renames, lets us reconcile after
  crashes and after the user moves files around in the Drive web UI.

`crates/driven-drive/src/remote_store.rs`:

```rust
use async_trait::async_trait;
use bytes::Bytes;

#[derive(Debug, Clone)]
pub struct RemoteEntry {
    pub id: String,
    pub name: String,
    pub parents: Vec<String>,
    pub size: Option<u64>,
    pub md5: Option<[u8; 16]>,                  // md5 of bytes actually stored on Drive
    pub mime_type: String,
    pub modified_time: i64,                     // unix ms
    pub trashed: bool,
    pub app_properties: HashMap<String, String>,
}

pub struct ResumableSession {
    pub url: String,                            // session URL Drive issues
    pub issued_at: i64,                         // we discard sessions older than 6 days (Drive expires at 7)
    pub size: u64,                              // total content length
    pub kind: ResumableKind,                    // Create or Update
}

#[derive(Debug)]
pub enum ResumableKind {
    Create { parent_id: String, name: String, app_properties: HashMap<String, String> },
    Update { file_id: String },
}

#[derive(Debug)]
pub enum ResumeProgress {
    InProgress { received: u64 },
    Completed(RemoteEntry),
    SessionInvalid,                             // 4xx during chunk; caller must restart
}

pub struct DownloadStream(pub Box<dyn tokio::io::AsyncRead + Send + Unpin>);

pub enum UploadBody {
    /// In-memory body (small files: ≤ RESUMABLE_THRESHOLD).
    Bytes(Bytes),
    /// Streaming body for the 3-stage pipeline (DESIGN §11.4.3).
    /// `len` is the total content length (required for resumable Content-Length headers).
    /// `stream` yields Bytes chunks; each chunk passed to Drive is sized to
    /// a multiple of 256 KiB except the final one (the executor accumulates
    /// pipeline-chunks to satisfy that).
    Stream {
        len: u64,
        stream: Box<dyn futures::Stream<Item = anyhow::Result<Bytes>> + Send + Unpin>,
    },
}

#[async_trait]
pub trait RemoteStore: Send + Sync {
    /// Ensure a child folder with the given name exists under `parent_id`
    /// **uniquely**. Searches by name; if multiple matches, picks the one
    /// with our `app_properties["driven.folder_marker"]` if present, else
    /// the oldest non-trashed one and logs a warning. Creates if none.
    async fn ensure_folder(&self, parent_id: &str, name: &str) -> anyhow::Result<RemoteEntry>;

    async fn list_folder(&self, folder_id: &str) -> anyhow::Result<Vec<RemoteEntry>>;

    /// CREATE a new file under `parent_id` with the given name. Always POST.
    /// Caller is responsible for ensuring there's no existing file_id for
    /// this (source_id, relative_path) — otherwise this creates a duplicate.
    async fn create(
        &self,
        parent_id: &str,
        name: &str,
        mime: &str,
        body: UploadBody,
        app_properties: HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry>;

    /// UPDATE an existing file by file_id (`PATCH`). Use for any change to
    /// a file we've already uploaded. Caller MUST hold the file_id from
    /// `file_state.drive_file_id`, not look it up by name.
    async fn update(
        &self,
        file_id: &str,
        body: UploadBody,
        app_properties_patch: HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry>;

    /// Open a resumable upload session. `kind` chooses create vs update.
    async fn resumable_session(
        &self,
        kind: ResumableKind,
        mime: &str,
        size: u64,
    ) -> anyhow::Result<ResumableSession>;

    /// Push a chunk. **Non-final chunks MUST be a multiple of 256 KiB.**
    /// The final chunk (when offset + chunk.len() == session.size) may be
    /// any size including non-multiple. On 4xx the session is dead —
    /// returns `ResumeProgress::SessionInvalid`. The caller discards the
    /// session and re-creates from offset 0.
    async fn resume_chunk(
        &self,
        session: &ResumableSession,
        offset: u64,
        chunk: Bytes,
    ) -> anyhow::Result<ResumeProgress>;

    /// Move to trash. Idempotent: trashing an already-trashed file succeeds.
    /// 404 is treated as success (already-gone is the desired state).
    async fn trash(&self, file_id: &str) -> anyhow::Result<()>;

    async fn metadata(&self, file_id: &str) -> anyhow::Result<RemoteEntry>;
    async fn download(&self, file_id: &str) -> anyhow::Result<DownloadStream>;

    /// Find an existing file we previously created under this parent by the
    /// `driven.client_op_uuid` appProperty. Used by the reconciliation pass
    /// (§5.7) after a crash: if we recorded the create-intent in
    /// `pending_ops` but crashed before recording the file_id, we can find
    /// the orphaned remote object and adopt it instead of re-creating.
    async fn find_by_op_uuid(&self, parent_id: &str, op_uuid: &str) -> anyhow::Result<Option<RemoteEntry>>;

    /// quota / about info; cheap to call, used to surface "x of y used" in UI
    async fn about(&self) -> anyhow::Result<AboutInfo>;
}
```

Both `GoogleDriveStore` and `InMemoryRemoteStore` implement this. A shared
**contract-test suite** (`crates/driven-drive/tests/fake_contract.rs`)
runs the same scenarios against both — guarantees the fake stays faithful.
The contract suite explicitly exercises:
- Two `create` calls with the same parent+name produce TWO distinct files
  (Drive duplicate-name semantics).
- `update` round-trip preserves file_id and `appProperties`.
- `resume_chunk` on a session that's been 4xx-d returns `SessionInvalid`.
- Non-256-KiB-multiple non-final chunks reject at the trait layer.
- `find_by_op_uuid` returns `None` for a UUID never used; finds the
  unique object when used; returns the most-recent if duplicate (with a
  warning).

---

## 4. The OAuth flow

We use the [`oauth2` crate](https://crates.io/crates/oauth2) (well-maintained,
PKCE support, bring-your-own HTTP client and loopback handler) rather than
`yup-oauth2`. `yup-oauth2`'s `InstalledFlowAuthenticator` would also work
but bundles its own loopback HTTP server, which hides the consent step
from our wizard UI — we want to drive the loopback and emit progress
events to the webview ourselves.

`crates/driven-drive/src/google/oauth.rs`:

```rust
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken,
    PkceCodeChallenge, RedirectUrl, Scope, TokenResponse, TokenUrl,
    basic::BasicClient,
};

pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
}

pub async fn run_pkce_loopback_flow(
    client_id: &str,
    client_secret: &str,
    open_browser: impl FnOnce(&str) -> anyhow::Result<()>,
    progress_tx: tokio::sync::mpsc::Sender<OAuthProgress>,
) -> anyhow::Result<Tokens> {
    // Bind both 127.0.0.1 and [::1] on the same port if possible.
    // Google's docs allow either loopback address for installed-app
    // redirect URIs (see RFC 8252 §7.3). Browsers on some user setups
    // resolve `localhost` to ::1 and the IPv4-only bind fails silently
    // with "connection refused". We pick a port that can bind on both,
    // and fall back to IPv4-only with a logged note if dual-bind fails.
    let (listener_v4, listener_v6, port) = bind_dual_loopback().await?;
    // The redirect URI we register with Google MUST be one literal
    // string and Google compares it byte-for-byte. We always use the
    // IPv4 form (127.0.0.1) — the v6 socket is for browsers that
    // happen to resolve `localhost` to ::1 and then issue the request
    // against ::1; we accept either and validate the Host header
    // matches the *exact* registered redirect URI host:port.
    let redirect_uri = format!("http://127.0.0.1:{port}/oauth/callback");

    // oauth2 v5 API: BasicClient::new takes only ClientId; the rest are
    // builder calls. (v4's positional-arg constructor was removed.)
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())  // prevent SSRF via OAuth-server redirect
        .build()?;
    let client = BasicClient::new(ClientId::new(client_id.into()))
        .set_client_secret(ClientSecret::new(client_secret.into()))
        .set_auth_uri(AuthUrl::new("https://accounts.google.com/o/oauth2/v2/auth".into())?)
        .set_token_uri(TokenUrl::new("https://oauth2.googleapis.com/token".into())?)
        .set_redirect_uri(RedirectUrl::new(redirect_uri.clone())?);

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    let (auth_url, csrf_state) = client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new("https://www.googleapis.com/auth/drive".into()))
        .add_extra_param("access_type", "offline")
        .add_extra_param("prompt", "consent")
        .set_pkce_challenge(pkce_challenge)
        .url();

    progress_tx.send(OAuthProgress::OpeningBrowser).await.ok();
    open_browser(auth_url.as_str())?;

    let code = wait_for_code(listener_v4, listener_v6, csrf_state.secret()).await?;

    progress_tx.send(OAuthProgress::ExchangingCode).await.ok();
    // oauth2 v5 API: request_async takes a borrowed reqwest::Client.
    // (v4's oauth2::reqwest::async_http_client wrapper was removed.)
    let token = client
        .exchange_code(AuthorizationCode::new(code))
        .set_pkce_verifier(pkce_verifier)
        .request_async(&http)
        .await?;

    let refresh = token.refresh_token()
        .ok_or_else(|| anyhow::anyhow!("Google returned no refresh token; user must revoke + reauth with prompt=consent"))?
        .secret()
        .to_string();
    Ok(Tokens {
        access_token: token.access_token().secret().into(),
        refresh_token: refresh,
        expires_at: token.expires_in()
            .map(|d| now_unix() + d.as_secs() as i64)
            .unwrap_or(now_unix() + 3600),
    })
}
```

`wait_for_code(listener_v4, listener_v6, csrf_state.secret())` runs a
one-shot `axum` server on **both** bound listeners (v4 and v6) on the
same port that:
- accepts `GET /oauth/callback?code=...&state=...`;
- validates `state` matches the CSRF token returned by `authorize_url`
  (constant-time comparison) — defends against an attacker browser tab
  redirecting their own code to our loopback;
- validates `Host:` matches the **exact registered** redirect URI's
  authority — i.e. `127.0.0.1:<our-port>`. Reject `localhost:<port>`
  even when the request came in over the v6 socket (defends against
  DNS-rebinding / browser-extension proxy attacks; the user's browser
  reaching our loopback only goes through `localhost` if they typed
  `localhost` themselves, which we explicitly do not register);
- serves a friendly HTML "you can close this tab" page;
- sends the code back to the caller via a oneshot channel;
- shuts down both listeners.

### 4.1 Token refresh

A thin `RefreshingTokenSource` wraps `Tokens` + the client and refreshes
when `expires_at - now < 60s`. POSTs to
`https://oauth2.googleapis.com/token` with `grant_type=refresh_token`.
If refresh returns `invalid_grant`, we mark the account `needs_reauth`
(SPEC §24 `auth.invalid_grant`).

Refresh tokens persist in the keychain only; the access token lives in
memory and is regenerated on demand.

### 4.2 No bundled re-implementation

We do **not** use `yup-oauth2` or layer our keychain inside its
`TokenStorage` trait. Doing both was specified in an earlier draft and
created two parallel code paths for what is really one flow. The
`oauth2` crate plus a 50-line refresh wrapper is enough.

---

## 5. The sync orchestrator

`crates/driven-core/src/orchestrator.rs`:

```rust
pub struct Orchestrator {
    account_id: AccountId,
    remote: Arc<dyn RemoteStore>,
    state: Arc<StateRepo>,
    clock: Arc<dyn Clock>,
    power: Arc<dyn PowerSource>,
    pacer: Pacer,
    crypto: Option<SourceCryptoSuite>,
    activity: ActivityWriter,
    config: Arc<RwLock<OrchestratorConfig>>,
    state_machine: RwLock<OrchestratorState>,
    pause_signal: watch::Receiver<bool>,
    events: broadcast::Sender<OrchestratorEvent>,
}

#[derive(Clone, Debug)]
pub enum OrchestratorState {
    Idle { last_run_at: Option<i64> },
    PowerCheck,
    Scanning { source_id: SourceId, scanned: u64 },
    Planning { plan: PlanSummary },
    Executing { progress: ExecProgress },
    Verifying { sampled: u64, mismatches: u64 },
    Backoff { until: i64 },
    Paused { reason: PauseReason },
    Error { detail: ErrorDetail },
}

impl Orchestrator {
    pub async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        loop {
            self.wait_for_tick_or_signal().await;
            if !self.power_ok().await { self.transition_paused(PauseReason::Battery).await; continue; }
            if self.pause_signal.borrow().clone()  { self.transition_paused(PauseReason::Manual).await; continue; }
            for source in self.state.enabled_sources(self.account_id).await? {
                self.run_one_source(&source).await?;
            }
        }
    }

    async fn run_one_source(&self, source: &SourceRow) -> anyhow::Result<()> {
        self.transition(OrchestratorState::Scanning { source_id: source.id, scanned: 0 });
        let scan = scanner::scan(source, &self.state).await?;
        let plan = planner::plan(source, &scan, &self.state).await?;
        self.transition(OrchestratorState::Planning { plan: plan.summary() });
        if self.config.read().await.dry_run { self.activity.dry_run_summary(&plan).await; return Ok(()); }
        self.transition(OrchestratorState::Executing { progress: ExecProgress::zero() });
        executor::execute(&plan, &self.remote, &self.state, &self.pacer, &self.crypto, &self.activity).await?;
        if verify::due(source, self.clock.now()) { verify::run(source, &self.remote, &self.state).await?; }
        self.transition(OrchestratorState::Idle { last_run_at: Some(self.clock.now()) });
        Ok(())
    }
}
```

(The above is illustrative — names, error handling, and the exact
broadcast channel shape are at the implementer's discretion. The state
*names* and *transitions* are not.)

---

## 6. Scanner pseudocode

```rust
pub async fn scan(source: &SourceRow, state: &StateRepo) -> Result<ScanResult> {
    let walker = build_walker(source);   // ignore::WalkBuilder configured per source
    let mut seen = HashSet::<PathBuf>::new();
    let mut new_or_changed = Vec::new();
    let known: HashMap<PathBuf, FileStateRow> = state.load_source_file_state(source.id).await?;

    for entry in walker.into_iter().filter_map(Result::ok) {
        if !entry.file_type().map_or(false, |t| t.is_file()) { continue; }
        let rel = entry.path().strip_prefix(&source.local_path)?.to_path_buf();
        seen.insert(rel.clone());

        let meta = entry.metadata()?;
        let size = meta.len();
        let mtime = mtime_ns(&meta);

        match known.get(&rel) {
            Some(row) if row.size == size && row.mtime_ns == mtime => continue, // unchanged
            Some(_) | None => new_or_changed.push(LocalEntry { rel, size, mtime }),
        }
    }
    let deleted: Vec<PathBuf> = known.keys().filter(|p| !seen.contains(*p)).cloned().collect();
    Ok(ScanResult { new_or_changed, deleted })
}

fn build_walker(source: &SourceRow) -> ignore::Walk {
    let mut wb = ignore::WalkBuilder::new(&source.local_path);
    wb.git_ignore(source.respect_gitignore)
      .git_exclude(source.respect_gitignore)
      .git_global(source.respect_gitignore)
      .hidden(false);
    let mut overrides = ignore::overrides::OverrideBuilder::new(&source.local_path);
    for inc in &source.include_patterns { overrides.add(inc)?; }
    for exc in &source.exclude_patterns { overrides.add(&format!("!{exc}"))?; }
    wb.overrides(overrides.build()?);
    wb.build()
}
```

The `include_patterns` lets the user opt-back-in things gitignore would
exclude (e.g. `!.env`). The `exclude_patterns` lets them exclude things
gitignore would include.

---

## 7. Planner pseudocode

```rust
pub async fn plan(source: &SourceRow, scan: &ScanResult, state: &StateRepo) -> Result<Plan> {
    let mut ops = Vec::new();

    for entry in &scan.new_or_changed {
        ops.push(Op::HashThenUpload {
            source_id: source.id,
            relative_path: entry.rel.clone(),
            size: entry.size,
        });
    }
    for path in &scan.deleted {
        let row = state.get_file_state(source.id, path).await?.expect("must exist");
        if let Some(file_id) = row.drive_file_id {
            ops.push(Op::Trash { source_id: source.id, relative_path: path.clone(), drive_file_id: file_id });
        } else {
            state.delete_file_state(source.id, path).await?;  // never made it to Drive
        }
    }
    Ok(Plan { ops })
}
```

---

## 8. Executor pseudocode

```rust
pub async fn execute(
    plan: &Plan,
    remote: &dyn RemoteStore,
    state: &StateRepo,
    pacer: &Pacer,
    crypto: &Option<SourceCryptoSuite>,
    activity: &ActivityWriter,
    pool: &UploadPool,                  // per-account; see DESIGN §11.4.2
) -> Result<()> {
    let mut joinset = JoinSet::new();
    for op in &plan.ops {
        // Both gates must be open: pool permit (concurrency) AND pacer permit (rate).
        let pool_permit = pool.acquire_owned().await?;
        let task = match op {
            Op::HashThenUpload { .. } => hash_then_upload(op.clone(), remote, state, pacer, crypto, activity, pool_permit),
            Op::Trash { .. }          => trash_op(op.clone(), remote, state, pacer, activity, pool_permit),
        };
        joinset.spawn(task);
    }
    while let Some(res) = joinset.join_next().await {
        res??;
    }
    Ok(())
}
```

`hash_then_upload` for files above the small-file threshold runs the
3-stage pipeline described in DESIGN §11.4.3:

```rust
async fn hash_then_upload_pipelined(op: Op, ...) -> Result<()> {
    // ... fstat identity captured pre-open (as in the single-task variant above) ...
    let (read_tx, read_rx) = mpsc::channel::<Bytes>(4);   // reader → cpu
    let (cpu_tx, cpu_rx)   = mpsc::channel::<Bytes>(4);   // cpu → uploader

    let reader = tokio::spawn(reader_loop(file, read_tx));
    let cpu = if file_size > BIG_FILE_THRESHOLD {
        rayon::spawn(|| pipeline_cpu_loop_rayon(read_rx, cpu_tx, crypto, hashers))
    } else {
        tokio::spawn(pipeline_cpu_loop(read_rx, cpu_tx, crypto, hashers))
    };
    let uploader = tokio::spawn(uploader_loop(cpu_rx, remote, resumable_session));

    let (read_res, cpu_res, upload_res) = tokio::try_join!(reader, cpu, uploader)?;
    // ... post-upload fstat identity check + md5 verify + state update ...
}
```

For files smaller than the threshold (default 4 MiB), the simpler
single-task variant runs everything inline — pipeline overhead would
dominate.

`hash_then_upload` is where the streaming-hash-on-read happens. Three
defenses against file-changed-during-upload (without these, the file
may be silently incoherent after upload):

1. **Pre-open `lstat`** captures `(dev, inode/file-index, size, mtime,
   ctime)` of the path entry.
2. **Open with `FILE_SHARE_DELETE`** (Windows) so another process can
   atomically replace it. The file handle continues to read the
   *original* unlinked bytes — we MUST detect the atomic-replace case
   and discard the result.
3. **Post-read `fstat`** the open handle and `lstat` the path again.
   If `fstat(handle).ctime/size` differs from the pre-open lstat, OR
   the path's current `(dev, inode)` no longer matches what we opened,
   the file was modified or atomically replaced mid-read. Abort: do
   NOT update `file_state`, do NOT mark `synced`. Re-enqueue for next
   scan. (The bytes already uploaded sit in a partial Drive object
   keyed to the resumable session; the executor's reconciliation pass
   (DESIGN §5.6) cleans those up.)

```rust
async fn hash_then_upload(op: Op, ...) -> Result<()> {
    let permit = pacer.permit().await;          // token bucket
    let full_path = source.local_path.join(&op.relative_path);
    let pre = lstat(&full_path)?;
    let mut file = open_with_share_delete(&full_path)?;
    let opened = fstat(&file)?;
    if (opened.dev, opened.file_index) != (pre.dev, pre.inode) {
        // someone replaced the file between our lstat and our open
        return Ok(SkipReason::ReplacedBeforeOpen.into());
    }
    let mut hasher_blake3 = blake3::Hasher::new();
    let mut hasher_md5 = md5::Md5::new();
    // Note: this is the local-read buffer. The HTTP upload chunk size is
    // independent — see resumable upload section (≥256 KiB enforced
    // there, this buffer can be any convenient size).
    let mut buf = vec![0u8; 64 * 1024];
    let mut len = 0u64;
    // … two-pass not needed — see comment below. For now we read twice if the
    // file is too large to buffer in RAM; small files we hash-and-buffer in one pass.

    // For large files we use resumable upload with on-the-fly hashing tee:
    let (sender, body_reader) = mpsc::channel::<Bytes>(8);
    let upload_fut = remote.resumable_session(parent, name, mime, size).and_then(|sess| {
        async move {
            let mut offset = 0u64;
            while let Some(chunk) = body_reader.recv().await {
                let progress = remote.resume_chunk(&sess, offset, chunk).await?;
                if let ResumeProgress::Completed(entry) = progress { return Ok(entry); }
                offset += chunk.len() as u64;
            }
            anyhow::bail!("stream ended before upload completed");
        }
    });
    let read_fut = async {
        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 { break; }
            hasher_blake3.update(&buf[..n]);
            hasher_md5.update(&buf[..n]);
            let chunk = encrypt_if_needed(&buf[..n], &mut state)?;
            sender.send(Bytes::copy_from_slice(&chunk)).await?;
            len += n as u64;
        }
        drop(sender);
        anyhow::Ok::<()>(())
    };
    let (read_res, upload_res) = tokio::join!(read_fut, upload_fut);
    read_res?;
    let entry = upload_res?;
    let local_hash = hasher_blake3.finalize();
    let local_md5 = hasher_md5.finalize();
    if Some(local_md5.into()) != entry.md5 {
        anyhow::bail!("md5 mismatch — Drive returned {:?}, local was {:?}", entry.md5, local_md5);
    }
    state.upsert_file_state(...).await?;
    activity.upload_done(...).await;
    Ok(())
}
```

(Sketch — production impl carefully handles `FILE_SHARE_DELETE`, retry
classification, per-chunk acks, and the encryption stream wrapping
the read tee.)

---

## 9. Pacer

AIMD (additive-increase, multiplicative-decrease) per-account token bucket.
Starts at 50 transactions/sec + 10 file-creates/sec. Halves on rate-limit
response; slowly raises ceiling on sustained-clean windows. Hard cap
configurable. **Independent of** the inter-file concurrency cap (DESIGN
§11.4.2) — pool size says "how many in flight"; pacer says "per second
budget". Both gates must be open for a request to issue.

**Bandwidth-cap enforcement** (settings.bandwidth_cap_mbps):
- When set, an additional `bytes_bucket` (refill =
  `bandwidth_cap_mbps * 1_000_000 / 8` bytes/sec, burst = 2× refill) is
  acquired in the reader-loop **before reading each chunk from disk**
  — this throttles the rate at which data enters the pipeline, which
  in turn back-pressures the uploader via the bounded mpsc channel.
- When unset (null), the bytes_bucket is None and acquire is a no-op.
- Bandwidth cap shares the per-account Pacer; cross-account uploads
  do not share a single byte budget (rare for personal use; if needed,
  V2 adds a global cap).

`crates/driven-core/src/pacer.rs`:

```rust
pub struct Pacer {
    qps_bucket: TokenBucket,           // 50/s initial, AIMD-adjusted
    file_bucket: TokenBucket,          // 10/s initial, AIMD-adjusted
    bytes_bucket: Option<TokenBucket>, // Some(...) when settings.bandwidth_cap_mbps is set;
                                       // refill rate = bandwidth_cap_mbps * 1_000_000 / 8 bytes/s;
                                       // None = unlimited (bypassed).
    backoff_until: AtomicI64,
    ceilings: RwLock<PacerCeilings>,
    last_rate_limit_at: AtomicI64,
}

impl Pacer {
    pub async fn permit_request(&self) {
        if let Some(wait) = self.backoff_remaining() { tokio::time::sleep(wait).await; }
        self.qps_bucket.acquire(1).await;
    }
    pub async fn permit_file_create(&self) {
        self.permit_request().await;
        self.file_bucket.acquire(1).await;
    }
    pub fn note_response(&self, classification: ResponseClass) {
        if let ResponseClass::RateLimited { retry_after } = classification {
            self.set_backoff(self.now() + retry_after_with_jitter(retry_after));
        }
    }
}
```

Buckets refill on a wall-clock schedule, not after-N-requests timer.

---

## 10. Power source

`crates/driven-power/src/lib.rs`:

```rust
#[async_trait]
pub trait PowerSource: Send + Sync {
    async fn current(&self) -> PowerState;
    fn subscribe(&self) -> broadcast::Receiver<PowerState>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PowerState {
    pub ac_connected: bool,
    pub battery_percent: Option<u8>,
    pub on_metered_network: bool,
    pub network_reachable: bool,
}
```

Per-platform impls poll at 30s and listen for events where the OS
provides them. `FakePowerSource` lets a test push state changes.

---

## 11. IPC commands (Rust ↔ Webview)

Naming: snake_case Rust, camelCase TS. Generated TS types live in
`ui/src/ipc/types.ts`, regenerated from Rust via a `cargo xtask
gen-ts` task (uses **tauri-specta** — it generates both TS types AND a typed `commands` wrapper for the frontend, so the frontend invokes `commands.list_accounts()` rather than untyped `invoke("list_accounts")`).

### 11.1 Accounts

```rust
#[tauri::command]
async fn list_accounts(state: State<'_, AppState>) -> Result<Vec<AccountDto>>;

#[tauri::command]
async fn begin_add_account_wizard(state: State<'_, AppState>) -> Result<AddAccountWizardSessionId>;

#[tauri::command]
async fn submit_oauth_credentials(state: State<'_, AppState>, session: SessionId, client_id: String, client_secret: String) -> Result<()>;

#[tauri::command]
async fn start_oauth_signin(state: State<'_, AppState>, session: SessionId) -> Result<OAuthAuthUrl>;

/// browser hits the loopback callback; the Rust side resolves the in-flight session
#[tauri::command]
async fn poll_oauth_status(state: State<'_, AppState>, session: SessionId) -> Result<OAuthStatus>;

#[tauri::command]
async fn finish_add_account(state: State<'_, AppState>, session: SessionId, display_name: Option<String>) -> Result<AccountDto>;

#[tauri::command]
async fn remove_account(state: State<'_, AppState>, account_id: AccountId, delete_remote: bool) -> Result<()>;

#[tauri::command]
async fn reauth_account(state: State<'_, AppState>, account_id: AccountId) -> Result<OAuthAuthUrl>;
```

### 11.2 Sources

```rust
#[tauri::command]
async fn list_sources(state: State<'_, AppState>) -> Result<Vec<SourceDto>>;

#[tauri::command]
async fn add_source(state: State<'_, AppState>, req: AddSourceRequest) -> Result<SourceDto>;

#[tauri::command]
async fn update_source(state: State<'_, AppState>, source_id: SourceId, patch: SourcePatch) -> Result<SourceDto>;

#[tauri::command]
async fn remove_source(state: State<'_, AppState>, source_id: SourceId, delete_remote: bool) -> Result<()>;

#[tauri::command]
async fn pick_drive_folder(state: State<'_, AppState>, account_id: AccountId, start_folder_id: Option<String>) -> Result<DriveFolderListing>;

#[tauri::command]
async fn preview_exclusions(state: State<'_, AppState>, req: ExclusionPreviewRequest) -> Result<ExclusionPreview>;
```

### 11.3 Sync

```rust
#[tauri::command]
async fn sync_now(state: State<'_, AppState>, source_id: Option<SourceId>) -> Result<()>;

#[tauri::command]
async fn pause_sync(state: State<'_, AppState>, duration_secs: Option<u64>) -> Result<()>;

#[tauri::command]
async fn resume_sync(state: State<'_, AppState>) -> Result<()>;

#[tauri::command]
async fn get_sync_status(state: State<'_, AppState>) -> Result<GlobalSyncStatus>;

#[tauri::command]
async fn dry_run(state: State<'_, AppState>, source_id: SourceId) -> Result<DryRunReport>;
```

### 11.4 Activity

```rust
#[tauri::command]
async fn query_activity(state: State<'_, AppState>, filter: ActivityFilter, page: PageRequest) -> Result<ActivityPage>;

#[tauri::command]
async fn clear_activity_older_than(state: State<'_, AppState>, before_ts: i64) -> Result<u64>;
```

### 11.5 Restore

```rust
#[tauri::command]
async fn list_remote_tree(state: State<'_, AppState>, source_id: SourceId, prefix: String) -> Result<Vec<RemoteEntryDto>>;

#[tauri::command]
async fn search_files(state: State<'_, AppState>, source_id: Option<SourceId>, query: String, limit: u32) -> Result<Vec<FileSearchHit>>;

#[tauri::command]
async fn restore_files(state: State<'_, AppState>, items: Vec<RestoreItem>, dest_dir: PathBuf) -> Result<RestoreJobId>;

#[tauri::command]
async fn get_restore_job(state: State<'_, AppState>, job: RestoreJobId) -> Result<RestoreJobStatus>;
```

### 11.6 Settings & misc

```rust
#[tauri::command]
async fn get_settings(state: State<'_, AppState>) -> Result<SettingsDto>;

#[tauri::command]
async fn update_settings(state: State<'_, AppState>, patch: SettingsPatch) -> Result<SettingsDto>;

#[tauri::command]
async fn export_diagnostic_bundle(state: State<'_, AppState>, dest: PathBuf) -> Result<PathBuf>;

#[tauri::command]
async fn check_for_updates(state: State<'_, AppState>) -> Result<Option<UpdateInfo>>;

#[tauri::command]
async fn list_releases(state: State<'_, AppState>, page: u32) -> Result<Vec<ReleaseDto>>;
```

### 11.6.1 IPC path validation (mandatory)

**Every IPC command that takes a `PathBuf` from the webview MUST validate
the path before any filesystem operation.** The webview must be treated
as untrusted with respect to the local filesystem — even though we ship
the frontend, a compromised render or a malicious browser extension could
shape IPC payloads.

The validation contract for every path-bearing command:

1. Resolve via `dunce::canonicalize` (Windows-friendly UNC handling) or
   `std::fs::canonicalize`. Reject non-existent intermediate components
   with a clear error.
2. **Confine to an allowed root.** Each command has an allow-list:
   - `restore_files(dest_dir)` — `dest_dir` must be under the user's
     home OR an explicitly-picked destination from a `tauri-plugin-dialog`
     dialog handle (we round-trip the dialog's returned path; the
     webview cannot inject an arbitrary path).
   - `export_diagnostic_bundle(dest)` — same dialog-handle rule.
   - `add_source.local_path` — must be from a dialog handle. If the
     webview supplies a string that wasn't dialog-derived, reject.
   - `list_remote_tree(prefix)` — the `prefix` here is a Drive-relative
     path, not local; validated as a printable UTF-8 string with `/`
     separators only, max-length-bounded.
3. **Reject paths containing `..` after canonicalisation** (path-traversal
   defense). Canonicalisation should already eat those; double-check.
4. **Reject symlink-traversal** at the leaf when the operation is a
   write (`restore_files`, `export_diagnostic_bundle`): use
   `symlink_metadata` on the leaf; if it's a symlink, refuse rather
   than dereference. Writes go through `O_NOFOLLOW`-equivalent open
   flags on Unix.
5. **Use atomic writes** for any payload the user might keep: write to
   `<dest>.driven-tmp.<random>` first, fsync, then atomically rename.
   Never leave a half-written file in place under a final name.

A reusable `validate_writable_dest(path, dialog_token) -> Result<PathBuf>`
helper enforces (1)-(4) for any command writing to disk. Tests in
`src-tauri/tests/ipc_path_validation.rs` cover: traversal attempt,
symlink-at-leaf, non-existent parent, path outside dialog handle, valid
case.

### 11.7 Events (Rust → webview)

Emitted via the Tauri v2 `Emitter` trait (`app.emit(event, payload)` for
broadcast; `app.emit_to(window_label, event, payload)` for targeted; the
v1 `emit_all` was removed):

| Event                  | Payload                            |
|------------------------|------------------------------------|
| `sync:status_changed`  | `GlobalSyncStatus`                 |
| `sync:source_progress` | `{ source_id, progress }`          |
| `activity:new`         | `ActivityEntry`                    |
| `account:needs_reauth` | `{ account_id, email }`            |
| `updater:available`    | `UpdateInfo`                       |
| `updater:downloaded`   | `UpdateInfo`                       |
| `restore:progress`     | `RestoreJobStatus`                 |
| `oauth:complete`       | `{ session_id, status }`           |

The webview subscribes via `@tauri-apps/api/event`.

---

## 12. Tray code

```rust
// src-tauri/src/tray.rs
pub fn build(app: &AppHandle) -> Result<()> {
    let tray = TrayIconBuilder::with_id("driven-main")
        .icon(idle_icon())
        .menu(&build_menu(app)?)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "sync_now"   => spawn(commands::sync_now(app.state(), None)),
            "settings"   => show_window(app, "main", Route::Settings),
            "activity"   => show_window(app, "main", Route::Activity),
            "restore"    => show_window(app, "main", Route::Restore),
            "pause_30m"  => spawn(commands::pause_sync(app.state(), Some(30 * 60))),
            "quit"       => app.exit(0),
            _            => {}
        })
        .on_tray_icon_event(|tray, event| match event {
            TrayIconEvent::Click { button: MouseButton::Left, .. } => {
                show_window(&tray.app_handle(), "main", Route::Activity);
            }
            _ => {}
        })
        .build(app)?;
    Ok(())
}
```

**Linux caveat from research:** click events on the tray icon may not
fire on every Linux desktop environment. We MUST gracefully degrade —
all functionality must be reachable from the right-click menu.

---

## 13. Auto-start

`src-tauri/src/main.rs`:

```rust
.plugin(tauri_plugin_autostart::init(
    tauri_plugin_autostart::MacosLauncher::LaunchAgent,
    Some(vec!["--minimized"]),
))
```

On first run, we ask in the wizard whether to enable autostart. Default
**off** so we don't surprise the user.

The `--minimized` arg skips opening a window; the app boots into the
tray only.

---

## 14. Single instance + deep-linking plugin order

**Plugin order matters in Tauri v2.** `tauri-plugin-single-instance`
must be registered **first** so that subsequent plugins (notably
`tauri-plugin-deep-link`) can hook into the on-second-launch callback
to forward URLs and CLI args to the already-running primary instance.
Registering deep-link before single-instance results in a second
process getting spawned that the primary never hears about.

```rust
tauri::Builder::default()
    // 1. single-instance FIRST — receives second-launch argv and CWD
    .plugin(tauri_plugin_single_instance::init(|app, argv, cwd| {
        show_window(app, "main", Route::default());
        handle_argv(app, argv);
    }))
    // 2. deep-link plugin SECOND — hooks into the single-instance callback
    .plugin(tauri_plugin_deep_link::init())
    .setup(|app| {
        // Tauri v2 deep-link API surface (not argv parsing):
        use tauri_plugin_deep_link::DeepLinkExt;
        let handle = app.handle().clone();
        app.deep_link().on_open_url(move |event| {
            for url in event.urls() {
                handle_driven_url(&handle, url);
            }
        });
        Ok(())
    })
```

`handle_argv` parses `--minimized` and `--restore <path>`. Deep-link
URLs are handled via the `on_open_url` callback (NOT argv parsing) —
Tauri v2's `tauri-plugin-deep-link` delivers them through that
callback on macOS (via the Apple event), on Windows/Linux via the
single-instance plugin's argv forwarding, transparently. URLs already
captured at process start are available synchronously via
`app.deep_link().get_current()`.

Runtime (dynamic) scheme registration is supported on Windows and
Linux only — macOS requires schemes declared at build time in
`tauri.conf.json` under `plugins.deep-link.desktop.schemes` and is only
testable on the bundled, installed application (not under
`cargo tauri dev`).

---

## 15. Updater

Two configuration concerns: **building** the updater artifacts in the
release pipeline, and **fetching** them at runtime.

### 15.1 Build-time: opt in to updater artifact generation

`tauri.conf.json`'s `bundle` block must enable `createUpdaterArtifacts`
so the build produces signed `.sig` files alongside the bundles. Without
this the release pipeline ships installers but no updater payload.

```jsonc
{
  "bundle": {
    "createUpdaterArtifacts": true,
    // ... other bundle config
  },
  "plugins": {
    "updater": {
      "active": true,
      "pubkey": "<base64 ed25519 public key>",
      "endpoints": [
        "https://driven.maxhogan.dev/updates/{{target}}/{{current_version}}/update.json"
      ],
      "dialog": false
    }
  }
}
```

`{{target}}`, `{{arch}}`, and `{{current_version}}` are the placeholders
Tauri's updater natively substitutes. **Channel (`stable` vs `dev`) is
NOT a supported placeholder** — earlier drafts of this spec used a
custom `{{channel}}` token; that was a mistake (Tauri silently treats
it as a literal string). The endpoint URL therefore embeds `{{arch}}`
(multi-arch builds — x86_64 vs aarch64 — would otherwise collide on a
single manifest URL).

### 15.2 Runtime: channel selection

Channel is picked at runtime via `tauri_plugin_updater::Builder::endpoints`
which accepts a `Vec<Url>` per-invocation. `src-tauri/src/updater.rs`
holds the wrapper:

```rust
fn build_updater(app: &AppHandle, channel: Channel) -> Updater {
    let endpoint = match channel {
        Channel::Stable => "https://driven.maxhogan.dev/updates/stable/{{target}}/{{current_version}}/update.json",
        Channel::Dev    => "https://driven.maxhogan.dev/updates/dev/{{target}}/{{current_version}}/update.json",
    };
    app.updater_builder()
        .endpoints(vec![endpoint.parse().expect("static URL")])
        .build()
        .expect("updater build")
}
```

The wrapper periodically calls `updater.check()` and, on availability,
emits an `updater:available` event. When the user clicks "Install
update" in the in-app banner, the wrapper calls
`update.download_and_install(progress_cb, ready_cb).await` and
relaunches.

### 15.3 Hosting

V0 host: `gh-pages` branch published as a static site. V1: Cloudflare
Pages on a Driven-owned domain (matches user's preferred stack — see
CLAUDE.md `valubot.ai` zone for the Unbroker pattern, this would be
`driven.maxhogan.dev/updates`).

---

## 16. Telemetry

`src-tauri/src/telemetry.rs`:

- Generates an install ID on first run, stores in `settings` table.
- On startup + every 24h, POSTs to `https://driven.maxhogan.dev/telemetry/v1/ping`
  (Cloudflare Worker → Analytics Engine).
- Payload schema:
  ```jsonc
  {
    "install_id": "uuid",
    "ts": 1750000000000,
    "version": "0.1.2",
    "os": "windows", "os_version": "11.26200", "arch": "x86_64",
    "channel": "stable",
    "events_24h": {
      "files_uploaded": 1234,
      "bytes_uploaded": 9876543,
      "errors_by_class": { "rate_limited": 4, "locked_file": 1 },
      "deep_verify_runs": 1,
      "update_applied": false
    },
    "latency_p50_p95_ms": { "scan": [12, 88], "upload_per_mb": [180, 920] }
  }
  ```
- Settings panel: toggle "Send anonymous usage stats" — default **on**,
  one click to disable.

---

## 17. Panic / crash handling

`src-tauri/src/panic_hook.rs`:

```rust
pub fn install() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write_crash_dump(info);
        prev(info);
    }));
}

fn write_crash_dump(info: &PanicHookInfo) {
    let path = log_dir().join(format!("crash-{}.txt", now_filename_safe()));
    let _ = std::fs::write(&path, format!("{info}\n{}", std::backtrace::Backtrace::force_capture()));
}
```

Crash files surface in the diagnostic bundle.

---

## 18. Diagnostic bundle

`export_diagnostic_bundle` produces a ZIP at the user-chosen path
containing:

- `version.txt`, `os.txt`
- `settings_redacted.json` — all settings minus account emails,
  client_ids, drive folder names (replaced by stable hashes).
- `schema.txt` — `PRAGMA user_version` + table counts.
- `activity_last_30d.csv` — `activity_log` table, **with paths and
  Drive file_ids replaced by stable per-bundle hashes** (`fileid_<hash>`,
  `path_<hash>`). The mapping is NOT included in the bundle.
- `logs/` — last 50 MB of tracing output from
  `<config_dir>/driven/logs/`, **after being passed through a redaction
  pipeline**:
  - OAuth tokens (anything matching the access / refresh token regex)
    replaced with `<token-redacted>`.
  - Local paths replaced with `<path:<hash>>`.
  - Drive file_ids replaced with `<fileid:<hash>>`.
  - Account emails replaced with `<email:<hash>>`.
  - Authorization headers in any error response stripped.
- `crashes/` — every `crash-*.txt` from the log dir, after the same
  redaction pipeline.
- `redaction-policy.txt` — explains exactly what was redacted and why,
  so the recipient knows the bundle's threat model.

**Caveat:** despite the redaction pipeline, log lines that happened to
include user-supplied free-text (e.g. a filename containing the word
"password" embedded in a path) may still carry incidental information.
We label the bundle "**reasonably safe to share with the Driven
maintainer**" — not "safe to publish to the internet". The user picks
where the zip lands and what they do with it; we never auto-upload.

---

## 19. CI workflows

### 19.1 `ci.yml`

Triggers: `pull_request`, `push: main`.

Jobs (matrix across `ubuntu-latest`, `macos-latest`, `windows-latest`):
1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace`  (uses `InMemoryRemoteStore`)
4. `cargo deny check`
5. UI: `pnpm install --frozen-lockfile && pnpm lint && pnpm test:unit && pnpm build`
6. Compile-only: `cargo tauri build --debug --no-bundle` to catch wiring breaks.

A separate job (linux only) runs the real-Drive e2e:
- Requires `DRIVEN_E2E_REFRESH_TOKEN` + `DRIVEN_E2E_DEST_FOLDER_ID` secrets (maintainer's own OAuth refresh token + a throwaway Drive folder under the maintainer's account). Exercises the production OAuth refresh code path that a service-account flow would miss.
- Runs `cargo test --test e2e_real -p driven-app -- --include-ignored`.

### 19.2 `release-please.yml`

```yaml
on:
  push:
    branches: [main]
permissions:
  contents: write
  pull-requests: write
jobs:
  release-please:
    runs-on: ubuntu-latest
    steps:
      - uses: googleapis/release-please-action@v4
        with:
          config-file: release-please-config.json
          manifest-file: .release-please-manifest.json
```

Maintains a "chore(main): release vX.Y.Z" PR with the changelog and version
bumps. Merging the PR creates the `v*` tag, which fires `release.yml`.

### 19.3 `release.yml`

Triggers: `push: tags: ['v*']`.

```yaml
jobs:
  build:
    strategy:
      fail-fast: false
      matrix:
        include:
          - { os: macos-latest, target: aarch64-apple-darwin }
          - { os: macos-latest, target: x86_64-apple-darwin }
          - { os: ubuntu-22.04, target: x86_64-unknown-linux-gnu }
          - { os: windows-latest, target: x86_64-pc-windows-msvc }
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { targets: ${{ matrix.target }} }
      - uses: pnpm/action-setup@v4
      - uses: actions/setup-node@v4
        with: { node-version: 20, cache: pnpm }
      - run: pnpm install --frozen-lockfile
      - uses: tauri-apps/tauri-action@v0
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          TAURI_SIGNING_PRIVATE_KEY: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
          TAURI_SIGNING_PRIVATE_KEY_PASSWORD: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY_PASSWORD }}
        with:
          tagName: ${{ github.ref_name }}
          releaseName: "Driven ${{ github.ref_name }}"
          releaseBody: "See CHANGELOG.md for details."
          releaseDraft: false
          args: --target ${{ matrix.target }}

  publish-updater-manifest:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - run: node scripts/generate-update-json.mjs stable
      - run: gh release upload ... # uploads the per-target update payloads
```

### 19.4 `dev-channel.yml`

Triggers (R9-P2-2 - GATED, NOT every main push): `workflow_dispatch` (manual, from
the Actions tab) OR a `push: main` whose HEAD commit message contains the literal
marker `[dev-build]`. A decision step gates the build on `event_name == dispatch ||
commit message contains [dev-build]`, so ordinary main commits (docs, refactors,
release-please commits - those ride the tag path in release.yml) do NOT spend
premium CI minutes on a dev installer.

Same build matrix as release.yml but:
- Version is `<next-patch>-dev.<run_number>.<sha>` (computed by
  `scripts/set-dev-version.mjs`): strictly ABOVE the current stable release and
  MONOTONIC across dev builds, so a stable user who opts into dev is always offered
  a strictly-newer dev build (the earlier `0.0.0-dev.<short-sha>` was LOWER than
  stable `0.1.0` and broke that). The build, publish-manifest, and GC jobs all
  derive the SAME value from the same checked-out commit.
- STAGE-THEN-PUBLISH (R4-P1-3): builds + uploads THIS run's run-unique assets to a
  rolling `dev` GitHub Release, then generates + validates all manifests, and only
  AFTER that succeeds GCs superseded assets - it never deletes before the rebuild
  validates (no broken-link window).
- Uploads to a `dev` GitHub Release ("rolling") via `softprops/action-gh-release`.
- Generates `dev/{{target}}/update.json` published to Cloudflare Pages under
  `/updates/dev/` (the other channel's live manifests are preserved by
  `scripts/fetch-live-channel.sh`, fail-closed).

---

## 20. `tauri.conf.json` skeleton

```jsonc
{
  "productName": "Driven",
  "version": "0.1.0",
  "identifier": "app.driven",
  "build": {
    "frontendDist": "../ui/dist",
    "devUrl": "http://localhost:5173",
    "beforeDevCommand": "pnpm --dir ../ui dev",
    "beforeBuildCommand": "pnpm --dir ../ui build"
  },
  "app": {
    "windows": [{
      "label": "main",
      "title": "Driven",
      "width": 1100,
      "height": 720,
      "visible": false,        // boot into tray; window opened on demand
      "decorations": true,
      "resizable": true,
      "minWidth": 880,
      "minHeight": 560
    }],
    "trayIcon": {
      "iconPath": "icons/tray.png",
      "iconAsTemplate": true   // macOS dark-mode aware
    },
    "security": {
      "csp": "default-src 'self'; img-src 'self' data:; connect-src 'self' ipc: tauri:"
    }
  },
  "bundle": {
    "active": true,
    "targets": ["app", "dmg", "msi", "nsis", "deb", "appimage"],
    "icon": ["icons/32x32.png", "icons/128x128.png", "icons/icon.icns", "icons/icon.ico"],
    "category": "Utility",
    "shortDescription": "One-way backup to Google Drive",
    "longDescription": "Driven backs up local folders to Google Drive, one way, fast, with encryption, scheduling, and battery awareness.",
    "publisher": "Driven",
    "macOS": { "minimumSystemVersion": "11.0" },
    "windows": { "wix": { "language": "en-US" } }
  },
  "plugins": {
    "updater": { /* see §15 */ },
    "deep-link": { "desktop": { "schemes": ["driven"] } }
  }
}
```

---

## 21. `justfile`

```just
default: dev

dev:
    cargo tauri dev

dev-seeded:
    cargo run --bin seed-fixtures
    DRIVEN_USE_FAKE_REMOTE=1 cargo tauri dev

test:
    cargo test --workspace
    cd ui && pnpm test:unit

test-e2e-fake:
    cargo test --test e2e_fake

test-e2e-real:
    cargo test --test e2e_real -- --include-ignored

watch:
    cargo watch -x 'test -p driven-core' -x 'test -p driven-drive'

lint:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cd ui && pnpm lint

bundle:
    cargo tauri build

clean:
    cargo clean
    cd ui && rm -rf dist node_modules

reset-state:
    rm -rf "$(driven config-dir)/state.db" "$(driven config-dir)/state.db-wal" "$(driven config-dir)/state.db-shm"
```

---

## 22. Settings schema

Stored as JSON values in the `settings` KV table.

```jsonc
// key: "global"
{
  "auto_start_on_login": false,
  "default_concurrent_uploads": null,   // null = auto: min(available_parallelism * 2, 16); user may override 1..=32
  "bandwidth_cap_mbps": null,            // null = unlimited
  "skip_on_battery": true,
  "skip_on_metered": true,
  "scan_interval_secs": 600,             // default 10 min
  "deep_verify_interval_secs": 604800,   // 7 days
  "io_priority": "low",                  // "normal" | "low" | "idle"
  "log_level": "info"
}

// key: "telemetry"
{
  "enabled": true,
  "install_id": "<uuid>",
  "endpoint": "https://driven.maxhogan.dev/telemetry/v1/ping"
}

// key: "updater"
{
  "channel": "stable",                   // "stable" | "dev"
  "check_interval_secs": 21600
}

// key: "windows" (Windows-only; absent on macOS/Linux)
{
  "vss_mode": "auto",                    // "auto" | "always" | "never"
                                         //   auto:   try direct open first, fall back to VSS on ERROR_SHARING_VIOLATION (default)
                                         //   always: snapshot the volume per cycle even for non-locked files (paranoid)
                                         //   never:  never use VSS; locked files always skipped + surfaced (no-elevate)
  "vss_helper": false                    // false (default) | true
                                         //   true:  route VSS snapshots through the least-privilege elevated helper
                                         //          (DESIGN s5.3.1) so the main app stays un-elevated; launches
                                         //          driven-vss-helper.exe via one UAC prompt on first use
                                         //   false: no helper; VSS needs the whole app launched as Administrator
}

// key: "ui"
{
  "tray_left_click_opens": "activity",
  "locale": "en-US",                     // user-overridable; defaults to OS locale on first run, falls back to en-US
  "color_mode": "system"                 // "system" | "light" | "dark"
}
```

---

## 23. Cargo workspace skeleton

`Cargo.toml` (workspace root):

```toml
[workspace]
resolver = "2"
members = [
  "crates/driven-core",
  "crates/driven-drive",
  "crates/driven-crypto",
  "crates/driven-power",
  "crates/driven-test-fixtures",
  "crates/driven-chaos",
  "src-tauri",                  # package name in src-tauri/Cargo.toml is "driven-app" so `cargo test -p driven-app` works
]

[workspace.package]
version = "0.1.0"
edition = "2021"
rust-version = "1.85"     # tested floor for CI; CI also tests against latest stable
authors = ["Driven contributors"]
license = "MIT OR Apache-2.0"
repository = "https://github.com/pmaxhogan/driven"

[workspace.dependencies]
tokio       = { version = "1", features = ["full"] }
anyhow      = "1"
thiserror   = "2"
serde       = { version = "1", features = ["derive"] }
serde_json  = "1"
tracing     = "0.1"
async-trait = "0.1"
bytes       = "1"
futures     = "0.3"

[profile.dev]
opt-level = 1            # faster runtime in dev tests at modest compile cost

[profile.release]
lto = "thin"
codegen-units = 1
strip = "symbols"
```

---

## 24. Error taxonomy

Each library crate defines a `thiserror`-derived error enum.
At the IPC boundary, errors are converted to a stable JSON shape:

```jsonc
{
  "code": "drive.rate_limited",   // dotted, stable
  "message": "Drive rate-limited the request",
  "retry_after_ms": 1200,         // optional, by code
  "details": { /* anything */ }
}
```

Stable codes (V1):

| Code                        | Meaning                                        |
|-----------------------------|------------------------------------------------|
| `auth.invalid_grant`         | Refresh token revoked; reauth required        |
| `auth.consent_required`      | First-time auth or scope change               |
| `auth.network_unreachable`   | Couldn't reach accounts.google.com            |
| `drive.rate_limited`         | 429 / userRateLimitExceeded                   |
| `drive.daily_quota_exhausted`| 403 dailyLimitExceeded — paused until reset   |
| `drive.quota_exhausted`      | 403 storageQuotaExceeded (user's Drive full)  |
| `drive.upload_size_limit`    | File exceeds Drive's per-file size limit      |
| `drive.checksum_mismatch`    | Verification failed after upload              |
| `drive.unreachable`          | Drive API down / unreachable / 5xx-circuit-open |
| `drive.resumable_session_invalid` | 4xx during resumable upload — caller must restart session |
| `local.file_locked`          | Couldn't open even with `FILE_SHARE_DELETE` (V1: locked file; VSS path failed too — see `local.vss_unavailable`) |
| `local.vss_unavailable`      | Driven needs elevation to use VSS but isn't elevated |
| `local.file_changed_during_upload` | Pre/post fstat showed file mutated mid-upload — re-queued |
| `local.file_replaced_during_upload` | Atomic-replace detected by inode identity check — re-queued |
| `local.io_error`             | Generic disk error                            |
| `local.path_too_long`        | OS path-length limit hit                      |
| `local.unicode_collision`    | Two distinct paths normalise to the same NFC string |
| `net.offline`                | OS reports no network connectivity            |
| `net.no_internet`            | Connected but generate-204 probe fails        |
| `net.dns_failed`             | Resolver returned no answer for a known-good domain |
| `net.captive_portal`         | Captive portal detected; user action required |
| `net.timeout`                | Request exceeded its configured timeout       |
| `net.intermittent`           | Circuit-breaker tripped after N failures      |
| `net.proxy_required`         | 407 from HTTP proxy / proxy auth needed       |
| `update.endpoint_unreachable`| driven.maxhogan.dev/updates unreachable                |
| `update.signature_invalid`   | Tauri updater signature verification failed   |
| `crypto.key_missing`         | Keychain entry not found                      |
| `crypto.decrypt_failed`      | AEAD verification failed                      |
| `crypto.recovery_phrase_invalid` | BIP39 input failed checksum               |
| `state.db_locked`            | SQLite locked (transient)                     |
| `state.db_corrupt`           | SQLite integrity_check failed; rebuild from Drive backup advised |
| `state.reconcile_orphan`     | Startup found a remote object without a local row — adopted/cleaned |
| `local.disk_full`            | Source filesystem out of space during a verify-style read or restore write |
| `local.invalid_filename`     | A name the local OS allowed but Drive will reject (reserved name, trailing dot/space, etc.) |
| `local.ads_skipped`          | NTFS Alternate Data Stream encountered; main stream backed up, ADS skipped |
| `drive.dest_folder_missing`  | The configured destination folder was deleted from Drive by the user |
| `drive.dest_folder_permission_denied` | Destination folder's sharing changed to read-only for this account |
| `harness.timeout`            | A stress-harness scenario exceeded its budget (chaos crate only) |
| `internal.bug`               | Programming error — please report             |

Frontend maps these to user-friendly messages via
`t('errors.${code}.short')` and `t('errors.${code}.long')` per DESIGN
§8.7. **Codes are load-bearing for i18n** — they are translation-bundle
keys. Renaming a code orphans every locale's translation for it. Codes
must therefore: (a) never change between minor versions; (b) only be
added, never removed; (c) deprecated codes stay translatable for at
least one major release.

---

## 25. Frontend route map

```
/setup                      SetupWizard.vue
/                           redirects to /activity
/activity                   Activity.vue
/sources                    Settings.vue (sources tab)
/accounts                   Settings.vue (accounts tab)
/rules                      Settings.vue (rules tab)
/about                      About.vue (version, updates, license)
/restore                    Restore.vue
/restore/:sourceId          Restore.vue (scoped)
```

---

## 26. What's deliberately *not* in the spec

These are left to the implementer (deliberately, so the agent has
room to make sensible code-level choices):

- Exact `Cargo.toml` for each crate (compose from §16 of DESIGN.md).
- Whether to use `sqlx` migrations vs `refinery` (recommend `sqlx`).
- Frontend component library or hand-rolled (recommend hand-rolled +
  Tailwind).
- Whether the tray icon uses bitmap or template SVG per platform
  (recommend SVG everywhere, with platform-aware tinting).
- Exact `release-please-config.json` shape (use `manifest` mode,
  Rust + Node packages tracked together).
- Choice of crash-reporting backend (recommend a local-only flow per the
  user's "diagnostic export button" preference, no Sentry).
- Whether `xtask` is used for code-gen (recommend yes, via `cargo run -p
  xtask -- gen-ts`).
