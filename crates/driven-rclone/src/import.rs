//! Translating one `rclone.conf` remote into Driven destination settings.
//!
//! [`classify`] is the whole surface: hand it an [`RcloneRemote`] and it
//! returns what Driven can make of it. Four outcomes:
//!
//! - [`RemoteImport::S3`] - a `type = s3` remote. Everything Driven's S3
//!   backend needs EXCEPT the bucket (see [`S3Remote`]).
//! - [`RemoteImport::Drive`] - a `type = drive` remote. The non-secret settings
//!   only; the OAuth grant cannot come along (see [`DriveRemote`]).
//! - [`RemoteImport::Sftp`] - a `type = sftp` remote. Host/port/username carry
//!   across; the password or private key never does (see [`SftpRemote`]).
//! - [`RemoteImport::Unsupported`] - anything else, with the reason spelled out.
//!
//! Nothing in this module performs I/O, so every mapping decision below is
//! covered by a plain unit test against a fixture.

use driven_s3::{S3Config, S3ConfigError, DEFAULT_REGION};
use driven_sftp::DEFAULT_PORT as SFTP_DEFAULT_PORT;

use crate::config_file::RcloneRemote;
use crate::secret::Secret;

/// rclone `provider` values whose backend does NOT accept path-style
/// addressing, so rclone talks virtual-host style to them.
///
/// Source: `backend/s3/provider/*.yaml` in rclone. A provider file that sets
/// `quirks.force_path_style: true` gets PATH style; one that omits the quirk
/// gets VIRTUAL-HOST style, because `setQuirks` in `backend/s3/s3.go` starts
/// from `virtualHostStyle = true` and only a quirk turns it off:
///
/// ```text
/// var virtualHostStyle = true
/// if provider.Quirks.ForcePathStyle != nil {
///     virtualHostStyle = !*provider.Quirks.ForcePathStyle
/// }
/// if virtualHostStyle || opt.UseAccelerateEndpoint {
///     opt.ForcePathStyle = false
/// }
/// ```
///
/// Note the last two lines: for a provider in THIS list rclone forces
/// virtual-host style even when the config says `force_path_style = true`. The
/// mapping below reproduces that, because the point of an importer is to
/// reproduce the addressing the user's remote actually works with.
///
/// Anything not listed here (including an unknown or newer provider name, and
/// an absent `provider`) maps to path style, which is both rclone's `Other`
/// default and Driven's own portable default.
const VIRTUAL_HOST_PROVIDERS: &[&str] = &[
    "AWS",
    "Alibaba",
    "Cubbit",
    "DigitalOcean",
    "Dreamhost",
    "GCS",
    "Hetzner",
    "HuaweiOBS",
    "ImpossibleCloud",
    "Intercolo",
    "Leviia",
    "Linode",
    "LyveCloud",
    "Netease",
    "OVHcloud",
    "Petabox",
    "Rabata",
    "RackCorp",
    "Scaleway",
    "Selectel",
    "Servercore",
    "Storj",
    "Synology",
    "TencentCOS",
    "Wasabi",
    "Zata",
    "us3",
];

/// A remark about one translated setting: something Driven inferred, defaulted,
/// or could not carry across. Rendered verbatim to the user.
///
/// A note NEVER contains a config value that could be a credential. Endpoints,
/// regions, bucket names and provider names are quoted freely; key material is
/// referred to by option name only.
pub type Note = String;

/// The reason a remote cannot be imported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsupportedReason {
    /// The section has no `type` at all - not a remote rclone would use.
    MissingType,
    /// A wrapper backend that decorates ANOTHER remote (`crypt`, `alias`,
    /// `chunker`, `combine`, `union`). The inner remote is the thing to import.
    Wrapper {
        /// The option naming the wrapped remote (`remote` / `upstreams`), when
        /// present, so the message can point at it.
        inner_option: Option<&'static str>,
    },
    /// rclone's NATIVE Backblaze B2 backend (`type = b2`). Driven speaks S3, and
    /// B2 also exposes an S3-compatible API, so this is a reconfigure rather
    /// than a hard no.
    NativeB2,
    /// A backend Driven has no destination for.
    NoDrivenBackend,
}

/// A `type = s3` remote translated into Driven's S3 destination settings.
///
/// ## The bucket is not in `rclone.conf`
///
/// rclone's s3 remote is a SERVICE, not a container: the bucket is part of the
/// path you type (`myremote:mybucket/some/dir`), never part of the config
/// section. So an imported remote is always missing exactly one required
/// [`S3Config`] field, and [`S3Remote::to_config`] takes it as an argument
/// rather than inventing one. This is the single biggest reason the CLI reports
/// an s3 remote as importable-with-one-more-answer instead of complete.
#[derive(Debug, Clone)]
pub struct S3Remote {
    /// The rclone remote name (the `[section]` header).
    pub remote_name: String,
    /// The `provider` value as written, or `None` when the config omits it.
    pub provider: Option<String>,
    /// The resolved endpoint: an absolute `http(s)` URL, ready for
    /// [`S3Config::endpoint`].
    pub endpoint: Option<String>,
    /// The resolved SigV4 signing region.
    pub region: String,
    /// The resolved addressing style for [`S3Config::path_style`].
    pub path_style: bool,
    /// `access_key_id` as written. `None` for an `env_auth` remote.
    pub access_key_id: Option<String>,
    /// `secret_access_key` as written, wrapped so it cannot be logged.
    pub secret_access_key: Option<Secret>,
    /// Per-setting remarks, in the order they were produced.
    pub notes: Vec<Note>,
    /// Things that must be answered before the destination can be created.
    /// Non-empty means "not importable as-is".
    pub blockers: Vec<Note>,
}

impl S3Remote {
    /// Build the [`S3Config`] Driven persists, given the bucket the user names.
    ///
    /// Fails when the endpoint could not be resolved, or when validation
    /// rejects the bucket / prefix.
    pub fn to_config(&self, bucket: &str, prefix: Option<&str>) -> anyhow::Result<S3Config> {
        let endpoint = self.endpoint.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "rclone.import_incomplete: remote {:?} has no endpoint and none could be derived",
                self.remote_name
            )
        })?;
        let cfg = S3Config {
            endpoint,
            bucket: bucket.to_string(),
            region: self.region.clone(),
            path_style: self.path_style,
            prefix: prefix.map(str::to_string),
        }
        .normalized()
        .map_err(|e: S3ConfigError| anyhow::anyhow!("{e}"))?;
        Ok(cfg)
    }

    /// Whether everything except the bucket carried across.
    pub fn is_complete(&self) -> bool {
        self.blockers.is_empty()
    }
}

/// A `type = drive` remote translated into what Driven can and cannot take.
///
/// ## Why the OAuth token is not here
///
/// rclone stores the Drive grant as a `token` option holding an
/// `{"access_token":..,"refresh_token":..,"expiry":..}` blob (plaintext - see
/// the crate docs on obscuring). That refresh token is bound to the OAuth
/// **client** that obtained it: rclone's own
/// `202264815644.apps.googleusercontent.com`, unless the user configured a
/// `client_id`/`client_secret` of their own.
///
/// This is a protocol rule, not a Google policy. RFC 6749 s6: "the refresh token
/// is bound to the client to which it was issued", and the authorization server
/// MUST "ensure that the refresh token was issued to the authenticated client".
/// s5.2 names the failure: `invalid_grant` covers a token that "was issued to
/// another client". Google's web-server OAuth guide requires `client_id` on
/// every refresh POST to `https://oauth2.googleapis.com/token` and defers to the
/// spec for grant semantics. So presenting rclone's refresh token under Driven's
/// client id fails - and note the error would be `invalid_grant`, not
/// `invalid_client`: Driven's own client authenticates fine, it is the GRANT
/// that does not belong to it.
///
/// This crate therefore never copies `token`. Driven's own PKCE loopback flow
/// (`driven-cli auth`, or the desktop wizard) mints a grant issued to Driven, in
/// seconds. The settings that ARE portable - which folder, which Shared Drive -
/// come across, so the only thing the user redoes is the consent click.
///
/// The nuance, stated honestly because it is the one case where a reviewer
/// would be right to push back: if the user configured rclone with their OWN
/// Google OAuth client, then that same client id/secret entered into Driven
/// WOULD make the refresh token redeemable. Driven still does not do it. The
/// grant would remain the one the user issued to "rclone" in their Google
/// account's third-party-access list (so revoking rclone silently breaks
/// Driven), and it carries rclone's requested scopes rather than Driven's. The
/// importer reports that a BYO client pair is present - that pair IS reusable,
/// being an app identity rather than a grant - and leaves the consent to
/// Driven's own flow.
#[derive(Debug, Clone)]
pub struct DriveRemote {
    /// The rclone remote name (the `[section]` header).
    pub remote_name: String,
    /// `root_folder_id` - the Drive folder id the remote is rooted at. Maps
    /// directly onto the destination folder Driven's source picker sets.
    pub root_folder_id: Option<String>,
    /// `team_drive` - the Shared Drive id. Maps onto Driven's per-source
    /// `drive_id` (issue #7); absent means My Drive.
    pub team_drive: Option<String>,
    /// `scope` as written, purely informational.
    pub scope: Option<String>,
    /// Whether the remote carries a BYO OAuth client id + secret. The VALUES are
    /// available via [`DriveRemote::client_id`] / [`DriveRemote::client_secret`]
    /// for a caller that explicitly asks; this flag is what a listing shows.
    pub has_byo_client: bool,
    /// Whether an OAuth `token` blob is present. Always reported, never copied.
    pub has_token: bool,
    /// Whether the remote authenticates as a service account
    /// (`service_account_file` / `service_account_credentials`).
    pub has_service_account: bool,
    /// Per-setting remarks.
    pub notes: Vec<Note>,
    /// What the user must do by hand. For Drive this is never empty - the OAuth
    /// flow is always required.
    pub blockers: Vec<Note>,
    byo_client_id: Option<String>,
    byo_client_secret: Option<Secret>,
}

impl DriveRemote {
    /// The BYO OAuth client id, when the remote has one.
    pub fn client_id(&self) -> Option<&str> {
        self.byo_client_id.as_deref()
    }

    /// The BYO OAuth client secret, when the remote has one.
    pub fn client_secret(&self) -> Option<&Secret> {
        self.byo_client_secret.as_ref()
    }
}

/// A `type = sftp` remote translated into what Driven can and cannot take.
///
/// ## The password / private key is never carried across
///
/// rclone stores `pass` "obscured" - AES-256-CTR under a hard-coded key baked
/// into rclone's own source (obfuscation, not encryption; see the crate docs'
/// "Obscured values" section). De-obscuring it would mean shipping rclone's
/// key inside Driven, turning a merely inconvenient value into a directly
/// usable one for anyone who reads `rclone.conf` off disk - the opposite of
/// what obscuring is for. And `key_file` in `rclone.conf` is only a PATH to a
/// private key on the machine that wrote the config, never the key material
/// itself - there is nothing here to import even in principle.
///
/// So the credential ALWAYS has to be re-entered by hand in Driven's setup
/// wizard, the same "settings carry across, the secret does not" split
/// [`DriveRemote`] documents for the OAuth grant - which is why, like
/// [`DriveRemote`], this type carries no `is_complete`/`to_config`: there is
/// no path to a one-step import.
///
/// ## The root path is not in `rclone.conf` either
///
/// Same shape as [`S3Remote`]: rclone's sftp remote is a SERVICE (host + auth
/// only), and the remote PATH is part of what you type at the CLI
/// (`myremote:/some/dir`), never part of the config section. The rendering
/// asks the caller for it, the same way [`render::RenderOptions::bucket`]
/// does for S3.
///
/// [`render::RenderOptions::bucket`]: crate::render::RenderOptions::bucket
#[derive(Debug, Clone)]
pub struct SftpRemote {
    /// The rclone remote name (the `[section]` header).
    pub remote_name: String,
    /// `host` as written, or `None` when absent (always a blocker: there is
    /// nothing to connect to without it).
    pub host: Option<String>,
    /// The resolved port. Falls back to [`SFTP_DEFAULT_PORT`] (22) when `port`
    /// is absent or does not parse as one.
    pub port: u16,
    /// `user` as written, or `None` when absent (always a blocker).
    pub user: Option<String>,
    /// Whether a `pass` option is present. The VALUE is never read - see the
    /// type docs.
    pub has_password: bool,
    /// The `key_file` PATH as written, purely informational: the file's
    /// contents are never read by this crate.
    pub key_file: Option<String>,
    /// Per-setting remarks, in the order they were produced.
    pub notes: Vec<Note>,
    /// Things that must be answered before the destination can be created.
    /// NEVER empty for this type - see the type docs.
    pub blockers: Vec<Note>,
}

/// What Driven can make of one rclone remote.
#[derive(Debug, Clone)]
pub enum RemoteImport {
    /// An S3-compatible destination.
    S3(Box<S3Remote>),
    /// A Google Drive destination.
    Drive(Box<DriveRemote>),
    /// An SSH (SFTP) destination.
    Sftp(Box<SftpRemote>),
    /// Nothing Driven can target.
    Unsupported {
        /// The rclone remote name.
        remote_name: String,
        /// Its `type`, or the empty string when absent.
        remote_type: String,
        /// Why it cannot be imported.
        reason: UnsupportedReason,
    },
}

impl RemoteImport {
    /// The rclone remote name, whatever the outcome.
    pub fn remote_name(&self) -> &str {
        match self {
            RemoteImport::S3(r) => &r.remote_name,
            RemoteImport::Drive(r) => &r.remote_name,
            RemoteImport::Sftp(r) => &r.remote_name,
            RemoteImport::Unsupported { remote_name, .. } => remote_name,
        }
    }

    /// The Driven backend this maps to, or `None` when unsupported.
    pub fn backend_kind(&self) -> Option<driven_remote::backend::BackendKind> {
        match self {
            RemoteImport::S3(_) => Some(driven_remote::backend::BackendKind::S3),
            RemoteImport::Drive(_) => Some(driven_remote::backend::BackendKind::GoogleDrive),
            RemoteImport::Sftp(_) => Some(driven_remote::backend::BackendKind::Sftp),
            RemoteImport::Unsupported { .. } => None,
        }
    }

    /// A one-line human summary for a listing: what will happen if you import it.
    pub fn summary(&self) -> String {
        match self {
            RemoteImport::S3(r) if r.is_complete() => {
                "S3-compatible - imports fully (you supply the bucket)".to_string()
            }
            // No "see below": a listing prints one line per remote and then the
            // count, so the blockers are not on screen. Name the command that
            // shows them.
            RemoteImport::S3(r) => format!(
                "S3-compatible - imports partially; run `rclone import {}` for what is missing",
                r.remote_name
            ),
            RemoteImport::Drive(_) => {
                "Google Drive - imports partially (sign in to Driven to authorize)".to_string()
            }
            RemoteImport::Sftp(_) => {
                "SSH (SFTP) - imports partially (re-enter the password or key in Driven)"
                    .to_string()
            }
            RemoteImport::Unsupported {
                remote_type,
                reason,
                ..
            } => format!("not importable ({}): {}", type_label(remote_type), reason),
        }
    }
}

fn type_label(remote_type: &str) -> &str {
    if remote_type.is_empty() {
        "no type"
    } else {
        remote_type
    }
}

impl std::fmt::Display for UnsupportedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnsupportedReason::MissingType => write!(
                f,
                "the section has no `type` option, so it is not an rclone remote"
            ),
            UnsupportedReason::Wrapper { inner_option } => {
                write!(f, "this is a wrapper around another remote")?;
                if let Some(opt) = inner_option {
                    write!(f, " (see its `{opt}` option)")?;
                }
                write!(
                    f,
                    ". Import the underlying remote instead - Driven applies its own \
                     end-to-end encryption and chunking on top of whichever destination you pick"
                )
            }
            UnsupportedReason::NativeB2 => write!(
                f,
                "this is rclone's native Backblaze B2 backend. Driven talks S3, and B2 serves an \
                 S3-compatible API - point Driven's S3 destination at \
                 https://s3.<region>.backblazeb2.com with a B2 application key"
            ),
            UnsupportedReason::NoDrivenBackend => write!(
                f,
                "Driven backs up to Google Drive, S3-compatible storage, and SSH (SFTP) servers only"
            ),
        }
    }
}

/// Wrapper backends: they decorate another remote rather than being storage.
/// The value is the option naming what they wrap.
const WRAPPERS: &[(&str, &str)] = &[
    ("crypt", "remote"),
    ("alias", "remote"),
    ("chunker", "remote"),
    ("compress", "remote"),
    ("hasher", "remote"),
    ("combine", "upstreams"),
    ("union", "upstreams"),
];

/// Translate one rclone remote into Driven's terms.
pub fn classify(remote: &RcloneRemote) -> RemoteImport {
    let remote_type = remote.remote_type().unwrap_or_default();
    match remote_type.as_str() {
        "s3" => RemoteImport::S3(Box::new(classify_s3(remote))),
        "drive" => RemoteImport::Drive(Box::new(classify_drive(remote))),
        "sftp" => RemoteImport::Sftp(Box::new(classify_sftp(remote))),
        "" => RemoteImport::Unsupported {
            remote_name: remote.name.clone(),
            remote_type,
            reason: UnsupportedReason::MissingType,
        },
        "b2" => RemoteImport::Unsupported {
            remote_name: remote.name.clone(),
            remote_type,
            reason: UnsupportedReason::NativeB2,
        },
        other => {
            let reason = match WRAPPERS.iter().find(|(t, _)| *t == other) {
                Some((_, inner)) => UnsupportedReason::Wrapper {
                    inner_option: Some(inner),
                },
                None => UnsupportedReason::NoDrivenBackend,
            };
            RemoteImport::Unsupported {
                remote_name: remote.name.clone(),
                remote_type,
                reason,
            }
        }
    }
}

/// Whether an rclone boolean option is set. rclone writes `true` / `false`;
/// goconfig's `Bool` also accepts `1`/`0`/`yes`/`no`/`on`/`off`, so those are
/// honoured rather than silently read as `false`.
fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" | "t" | "y" => Some(true),
        "false" | "0" | "no" | "off" | "f" | "n" => Some(false),
        _ => None,
    }
}

fn classify_s3(remote: &RcloneRemote) -> S3Remote {
    let mut notes = Vec::new();
    let mut blockers = Vec::new();

    let provider = remote.get("provider").map(str::to_string);
    let provider_ref = provider.as_deref().unwrap_or("Other");

    // --- region ---------------------------------------------------------
    // rclone lets an S3-clone remote leave the region blank entirely; Driven
    // always signs with one, so a blank falls back to the same default the S3
    // backend documents. `location_constraint` is a bucket-CREATION setting
    // that rclone documents as "must be set to match the Region", so it is a
    // better guess than the default when the region itself is missing.
    let region = match remote.get("region") {
        Some(r) => r.to_string(),
        None => match remote.get("location_constraint") {
            Some(lc) => {
                notes.push(format!(
                    "no `region` set; using `location_constraint` ({lc}), which rclone documents as matching the region"
                ));
                lc.to_string()
            }
            None => {
                notes.push(format!(
                    "no `region` set; defaulting to {DEFAULT_REGION} (rclone signs S3 clones with an empty region, Driven always signs with one)"
                ));
                DEFAULT_REGION.to_string()
            }
        },
    };
    if region == "auto" {
        // Kept as-is rather than rewritten to us-east-1: `auto` is what the
        // user's working R2 remote signs with, and R2 accepts both.
        notes.push(
            "region `auto` carried across unchanged (Cloudflare R2 accepts it; do not reuse it for AWS)"
                .to_string(),
        );
    }

    // --- endpoint -------------------------------------------------------
    let endpoint = match remote.get("endpoint") {
        Some(raw) => Some(absolute_endpoint(raw, &mut notes)),
        None if provider_ref.eq_ignore_ascii_case("AWS") || provider.is_none() => {
            // rclone derives the AWS endpoint from the region, so a real AWS
            // remote usually has no `endpoint` line at all. Reconstruct the
            // regional endpoint rather than declaring the remote unimportable.
            if remote.get("region").is_some() || provider_ref.eq_ignore_ascii_case("AWS") {
                let derived = format!("https://s3.{region}.amazonaws.com");
                notes.push(format!(
                    "no `endpoint` set; derived the AWS regional endpoint {derived} from the region (rclone does the same internally)"
                ));
                Some(derived)
            } else {
                blockers.push(
                    "no `endpoint` and no `provider`/`region` to derive one from - enter the service endpoint by hand".to_string(),
                );
                None
            }
        }
        None => {
            blockers.push(format!(
                "no `endpoint` set, and one cannot be derived for provider `{provider_ref}` - enter the service endpoint by hand"
            ));
            None
        }
    };

    // --- addressing style ------------------------------------------------
    let provider_is_virtual_host = VIRTUAL_HOST_PROVIDERS
        .iter()
        .any(|p| p.eq_ignore_ascii_case(provider_ref));
    let explicit_path_style = remote.get("force_path_style").and_then(parse_bool);
    let path_style = if provider_is_virtual_host {
        if explicit_path_style == Some(true) {
            notes.push(format!(
                "`force_path_style = true` ignored: rclone forces virtual-host addressing for provider `{provider_ref}` regardless of the setting, so the import matches what actually works"
            ));
        }
        false
    } else {
        explicit_path_style.unwrap_or(true)
    };

    // --- credentials -----------------------------------------------------
    let access_key_id = remote.get("access_key_id").map(str::to_string);
    let secret_access_key = remote.get("secret_access_key").map(Secret::new);
    let env_auth = remote.get("env_auth").and_then(parse_bool).unwrap_or(false);
    match (&access_key_id, &secret_access_key) {
        (Some(_), Some(_)) => {}
        _ if env_auth => blockers.push(
            "`env_auth = true`: rclone reads this remote's credentials from the environment or an \
             IAM role at run time. Driven stores an explicit key pair in the OS keychain, so \
             create one and enter it in the destination form"
                .to_string(),
        ),
        _ => blockers.push(
            "no `access_key_id` / `secret_access_key` in the config (an anonymous or externally \
             credentialed remote); Driven needs a key pair that can write to the bucket"
                .to_string(),
        ),
    }
    if remote.get("session_token").is_some() {
        blockers.push(
            "`session_token` is set: these are TEMPORARY STS credentials that expire, and Driven \
             has no way to refresh them. Use a long-lived key pair for a backup destination"
                .to_string(),
        );
    }

    // --- settings Driven has no equivalent for ---------------------------
    for (key, what) in [
        ("storage_class", "storage class"),
        ("acl", "canned ACL"),
        ("server_side_encryption", "server-side encryption"),
        ("sse_kms_key_id", "SSE-KMS key"),
        ("sse_customer_algorithm", "SSE-C algorithm"),
    ] {
        if remote.get(key).is_some() {
            notes.push(format!(
                "`{key}` ({what}) is not carried across: Driven writes objects with the bucket's defaults and encrypts content itself before upload"
            ));
        }
    }

    S3Remote {
        remote_name: remote.name.clone(),
        provider,
        endpoint,
        region,
        path_style,
        access_key_id,
        secret_access_key,
        notes,
        blockers,
    }
}

/// Turn an rclone endpoint into an absolute URL.
///
/// rclone accepts (and its own provider examples list) BARE HOSTNAMES such as
/// `s3.wasabisys.com`; Driven's [`S3Config`] requires an absolute `http(s)` URL
/// and rejects anything else. Prepending `https://` is the only safe
/// completion - never `http://`, which would silently downgrade a backup's
/// transport.
fn absolute_endpoint(raw: &str, notes: &mut Vec<Note>) -> String {
    let raw = raw.trim();
    if raw.contains("://") {
        return raw.to_string();
    }
    notes.push(format!(
        "endpoint `{raw}` had no scheme (rclone allows a bare hostname); imported as https://{raw}"
    ));
    format!("https://{raw}")
}

fn classify_drive(remote: &RcloneRemote) -> DriveRemote {
    let mut notes = Vec::new();
    let mut blockers = Vec::new();

    let root_folder_id = remote.get("root_folder_id").map(str::to_string);
    let team_drive = remote.get("team_drive").map(str::to_string);
    let scope = remote.get("scope").map(str::to_string);
    let byo_client_id = remote.get("client_id").map(str::to_string);
    let byo_client_secret = remote.get("client_secret").map(Secret::new);
    let has_byo_client = byo_client_id.is_some();
    let has_token = remote.get("token").is_some();
    let has_service_account = remote.get("service_account_file").is_some()
        || remote.get("service_account_credentials").is_some();

    match &root_folder_id {
        Some(id) => notes.push(format!(
            "`root_folder_id` {id} carried across - use it as the destination folder when you add a backup source"
        )),
        None => notes.push(
            "no `root_folder_id`: the remote is rooted at the drive itself, so pick a destination folder in Driven"
                .to_string(),
        ),
    }
    if let Some(td) = &team_drive {
        notes.push(format!(
            "`team_drive` {td} carried across as the Shared Drive to scope the destination to"
        ));
    }
    if let Some(scope) = &scope {
        notes.push(format!(
            "`scope` was `{scope}`; Driven requests its own scopes during sign-in, so this is informational only"
        ));
    }

    // The load-bearing message. See the type docs for the RFC citation.
    blockers.push(
        "the OAuth `token` cannot be imported: a refresh token is redeemable only by the OAuth \
         client it was issued to (RFC 6749 s6; a cross-client refresh returns `invalid_grant`), \
         and this grant was issued to rclone's client, not Driven's. Sign in to Driven to \
         authorize this account - it takes one consent click"
            .to_string(),
    );
    if has_byo_client {
        notes.push(
            "this remote uses your OWN Google OAuth client (`client_id` / `client_secret`). That \
             PAIR is reusable - it identifies the app, not the grant - so you can enter it in \
             Driven's advanced sign-in and keep your own API quota (rclone is retiring its shared \
             client during 2026, so you already need one). The token still cannot come across: it \
             would leave Driven running on the consent you granted to rclone, which revoking \
             rclone would break"
                .to_string(),
        );
    }
    if has_service_account {
        blockers.push(
            "`service_account_file` / `service_account_credentials`: Driven authorizes as a signed-in \
             user through its OAuth flow and has no service-account mode, so this remote's identity \
             cannot be reused"
                .to_string(),
        );
    }
    if !has_token && !has_service_account {
        notes.push(
            "no `token` in the config - this remote was never authorized, or its credentials live elsewhere"
                .to_string(),
        );
    }

    DriveRemote {
        remote_name: remote.name.clone(),
        root_folder_id,
        team_drive,
        scope,
        has_byo_client,
        has_token,
        has_service_account,
        notes,
        blockers,
        byo_client_id,
        byo_client_secret,
    }
}

fn classify_sftp(remote: &RcloneRemote) -> SftpRemote {
    let mut notes = Vec::new();
    let mut blockers = Vec::new();

    let host = remote.get("host").map(str::to_string);
    if host.is_none() {
        blockers.push("no `host` set - the SFTP server address is required".to_string());
    }

    let port = match remote.get("port") {
        Some(raw) => match raw.trim().parse::<u16>() {
            Ok(p) => p,
            Err(_) => {
                notes.push(format!(
                    "`port` ({raw:?}) could not be parsed as a port number; defaulting to {SFTP_DEFAULT_PORT}"
                ));
                SFTP_DEFAULT_PORT
            }
        },
        None => SFTP_DEFAULT_PORT,
    };

    let user = remote.get("user").map(str::to_string);
    if user.is_none() {
        blockers.push("no `user` set - the SSH username is required".to_string());
    }

    let has_password = remote.get("pass").is_some();
    let key_file = remote.get("key_file").map(str::to_string);
    let key_use_agent = remote
        .get("key_use_agent")
        .and_then(parse_bool)
        .unwrap_or(false);

    match (has_password, &key_file) {
        (true, _) => notes.push(
            "a `pass` is configured, but rclone stores it obscured (obfuscated, not encrypted) \
             - Driven does not de-obscure it. Re-enter the password in Driven's setup wizard"
                .to_string(),
        ),
        (false, Some(path)) => notes.push(format!(
            "a private key is referenced at `{path}` on the machine that wrote this config; only \
             the PATH is in rclone.conf, never the key material - paste the key into Driven's \
             setup wizard"
        )),
        (false, None) if key_use_agent => notes.push(
            "`key_use_agent = true`: this remote authenticates via ssh-agent, which Driven does \
             not support in v1 - use a password or a pasted private key instead"
                .to_string(),
        ),
        (false, None) => notes.push(
            "no password or private key found in this remote's config - enter one in Driven's \
             setup wizard"
                .to_string(),
        ),
    }
    // Unconditional: whatever the state above, Driven never has a usable
    // credential to carry across - see the type docs.
    blockers.push(
        "the password or private key cannot be imported: rclone stores a password obscured \
         (not encrypted) and a private key only by file path, so re-enter your credential in \
         Driven's setup wizard - it takes one form"
            .to_string(),
    );

    SftpRemote {
        remote_name: remote.name.clone(),
        host,
        port,
        user,
        has_password,
        key_file,
        notes,
        blockers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_file::RcloneConfig;

    fn remote(text: &str) -> RcloneRemote {
        let cfg = RcloneConfig::parse(text).expect("fixture parses");
        cfg.remotes().first().expect("one remote").clone()
    }

    fn s3(text: &str) -> S3Remote {
        match classify(&remote(text)) {
            RemoteImport::S3(r) => *r,
            other => panic!("expected an s3 import, got {}", other.summary()),
        }
    }

    fn drive(text: &str) -> DriveRemote {
        match classify(&remote(text)) {
            RemoteImport::Drive(r) => *r,
            other => panic!("expected a drive import, got {}", other.summary()),
        }
    }

    // -----------------------------------------------------------------
    // Per-provider mapping. These five are the shapes `rclone config`
    // actually writes for the providers the README compares against, so a
    // regression here is a user whose imported destination does not work.
    // -----------------------------------------------------------------

    #[test]
    fn aws_derives_the_regional_endpoint_and_virtual_host_addressing() {
        // A real AWS remote has NO endpoint line - rclone builds it from the
        // region - and AWS is not in rclone's path-style quirk list.
        let r = s3("[aws]\ntype = s3\nprovider = AWS\naccess_key_id = AKIDEXAMPLE\nsecret_access_key = s3cret\nregion = us-west-2\n");
        assert_eq!(
            r.endpoint.as_deref(),
            Some("https://s3.us-west-2.amazonaws.com")
        );
        assert_eq!(r.region, "us-west-2");
        assert!(!r.path_style, "AWS is virtual-host style in rclone");
        assert!(r.is_complete(), "blockers: {:?}", r.blockers);
        let cfg = r.to_config("my-bucket", None).unwrap();
        assert_eq!(cfg.endpoint, "https://s3.us-west-2.amazonaws.com");
        assert_eq!(cfg.bucket, "my-bucket");
        assert!(!cfg.path_style);
    }

    #[test]
    fn cloudflare_r2_keeps_its_account_endpoint_auto_region_and_path_style() {
        let r = s3("[r2]\ntype = s3\nprovider = Cloudflare\naccess_key_id = k\nsecret_access_key = s\nendpoint = https://abc123.r2.cloudflarestorage.com\nregion = auto\n");
        assert_eq!(
            r.endpoint.as_deref(),
            Some("https://abc123.r2.cloudflarestorage.com")
        );
        assert_eq!(r.region, "auto", "R2 signs with `auto`; do not rewrite it");
        assert!(r.path_style, "Cloudflare quirks force_path_style = true");
        assert!(r.is_complete());
        assert!(
            r.notes.iter().any(|n| n.contains("auto")),
            "the `auto` region is worth a remark: {:?}",
            r.notes
        );
    }

    #[test]
    fn minio_keeps_a_plain_http_endpoint_and_path_style() {
        let r = s3("[minio]\ntype = s3\nprovider = Minio\naccess_key_id = minioadmin\nsecret_access_key = minioadmin\nendpoint = http://127.0.0.1:9000\n");
        assert_eq!(r.endpoint.as_deref(), Some("http://127.0.0.1:9000"));
        assert_eq!(r.region, DEFAULT_REGION, "MinIO leaves the region blank");
        assert!(r.path_style, "Minio quirks force_path_style = true");
        assert!(r.is_complete());
    }

    #[test]
    fn wasabi_gains_a_scheme_and_is_virtual_host_style() {
        // rclone's own Wasabi endpoint examples are BARE hostnames, and Wasabi
        // has no force_path_style quirk.
        let r = s3("[wasabi]\ntype = s3\nprovider = Wasabi\naccess_key_id = k\nsecret_access_key = s\nendpoint = s3.eu-central-1.wasabisys.com\nregion = eu-central-1\n");
        assert_eq!(
            r.endpoint.as_deref(),
            Some("https://s3.eu-central-1.wasabisys.com"),
            "a bare hostname must be completed with https, never http"
        );
        assert!(!r.path_style);
        assert!(r.is_complete());
        assert!(r.notes.iter().any(|n| n.contains("no scheme")));
    }

    #[test]
    fn backblaze_over_the_s3_api_imports_as_a_generic_provider() {
        // rclone has no `Backblaze` s3 provider, so a B2-over-S3 remote is
        // configured as `Other` (or with no provider) plus B2's endpoint.
        let r = s3("[b2s3]\ntype = s3\nprovider = Other\naccess_key_id = 004k\nsecret_access_key = K004s\nendpoint = https://s3.us-west-004.backblazeb2.com\nregion = us-west-004\n");
        assert_eq!(
            r.endpoint.as_deref(),
            Some("https://s3.us-west-004.backblazeb2.com")
        );
        assert_eq!(r.region, "us-west-004");
        assert!(r.path_style, "the `Other` provider quirks force_path_style");
        assert!(r.is_complete());
    }

    #[test]
    fn the_native_b2_backend_is_reported_as_a_reconfigure_not_a_dead_end() {
        let out = classify(&remote("[b2]\ntype = b2\naccount = 123\nkey = abc\n"));
        match &out {
            RemoteImport::Unsupported { reason, .. } => {
                assert_eq!(*reason, UnsupportedReason::NativeB2);
                assert!(reason.to_string().contains("backblazeb2.com"));
            }
            _ => panic!("b2 must not classify as importable"),
        }
        assert!(out.backend_kind().is_none());
    }

    // -----------------------------------------------------------------
    // Addressing-style resolution
    // -----------------------------------------------------------------

    #[test]
    fn an_explicit_force_path_style_is_honoured_for_a_path_style_provider() {
        let r = s3("[m]\ntype = s3\nprovider = Minio\naccess_key_id = k\nsecret_access_key = s\nendpoint = https://e.example\nforce_path_style = false\n");
        assert!(!r.path_style, "the user opted into virtual-host style");
    }

    #[test]
    fn force_path_style_true_is_overridden_for_a_virtual_host_provider_with_a_note() {
        // rclone's setQuirks clobbers ForcePathStyle for these providers, so
        // honouring the config literally would produce a destination that does
        // not work where rclone's did.
        let r = s3("[a]\ntype = s3\nprovider = AWS\naccess_key_id = k\nsecret_access_key = s\nregion = eu-west-1\nforce_path_style = true\n");
        assert!(!r.path_style);
        assert!(
            r.notes.iter().any(|n| n.contains("ignored")),
            "the override must be explained: {:?}",
            r.notes
        );
    }

    #[test]
    fn an_unknown_provider_defaults_to_path_style() {
        let r = s3("[x]\ntype = s3\nprovider = SomeNewProvider2029\naccess_key_id = k\nsecret_access_key = s\nendpoint = https://e.example\n");
        assert!(
            r.path_style,
            "an unknown provider must land on the portable default"
        );
    }

    #[test]
    fn rclone_boolean_spellings_are_all_understood() {
        for (raw, expected) in [
            ("true", true),
            ("TRUE", true),
            ("1", true),
            ("yes", true),
            ("on", true),
            ("false", false),
            ("0", false),
            ("no", false),
            ("off", false),
        ] {
            assert_eq!(parse_bool(raw), Some(expected), "{raw:?}");
        }
        assert_eq!(parse_bool("maybe"), None);
        // An unparseable value falls back to the provider default rather than
        // silently reading as `false`.
        let r = s3("[m]\ntype = s3\nprovider = Minio\naccess_key_id = k\nsecret_access_key = s\nendpoint = https://e.example\nforce_path_style = maybe\n");
        assert!(r.path_style);
    }

    // -----------------------------------------------------------------
    // Region + endpoint edge cases
    // -----------------------------------------------------------------

    #[test]
    fn location_constraint_stands_in_for_a_missing_region() {
        let r = s3("[c]\ntype = s3\nprovider = Ceph\naccess_key_id = k\nsecret_access_key = s\nendpoint = https://ceph.example\nlocation_constraint = eu-central-1\n");
        assert_eq!(r.region, "eu-central-1");
        assert!(r.notes.iter().any(|n| n.contains("location_constraint")));
    }

    #[test]
    fn a_missing_endpoint_for_a_non_aws_provider_is_a_blocker_not_a_guess() {
        let r = s3("[c]\ntype = s3\nprovider = Ceph\naccess_key_id = k\nsecret_access_key = s\nregion = eu\n");
        assert!(r.endpoint.is_none());
        assert!(!r.is_complete());
        assert!(r.blockers.iter().any(|b| b.contains("endpoint")));
        assert!(
            r.to_config("bucket", None).is_err(),
            "to_config must refuse to invent an endpoint"
        );
    }

    #[test]
    fn a_bare_provider_less_remote_with_a_region_still_derives_an_aws_endpoint() {
        let r =
            s3("[a]\ntype = s3\naccess_key_id = k\nsecret_access_key = s\nregion = ap-south-1\n");
        assert_eq!(
            r.endpoint.as_deref(),
            Some("https://s3.ap-south-1.amazonaws.com")
        );
        assert!(r.is_complete());
    }

    #[test]
    fn a_remote_with_nothing_but_a_type_reports_every_blocker_at_once() {
        let r = s3("[empty]\ntype = s3\n");
        assert!(r.endpoint.is_none());
        assert!(!r.is_complete());
        assert!(
            r.blockers.len() >= 2,
            "endpoint AND credentials are both missing: {:?}",
            r.blockers
        );
    }

    #[test]
    fn to_config_normalizes_the_prefix_and_rejects_a_bad_bucket() {
        let r = s3("[m]\ntype = s3\nprovider = Minio\naccess_key_id = k\nsecret_access_key = s\nendpoint = http://127.0.0.1:9000/\n");
        let cfg = r.to_config("bkt", Some("/backups/laptop")).unwrap();
        assert_eq!(
            cfg.endpoint, "http://127.0.0.1:9000",
            "trailing slash trimmed"
        );
        assert_eq!(cfg.prefix.as_deref(), Some("backups/laptop/"));
        assert!(r.to_config("a/b", None).is_err(), "a bucket with a slash");
        assert!(r.to_config("", None).is_err(), "an empty bucket");
    }

    // -----------------------------------------------------------------
    // Credentials
    // -----------------------------------------------------------------

    #[test]
    fn env_auth_is_a_blocker_with_an_actionable_message() {
        let r = s3("[a]\ntype = s3\nprovider = AWS\nregion = us-east-1\nenv_auth = true\n");
        assert!(r.access_key_id.is_none());
        assert!(!r.is_complete());
        assert!(r.blockers.iter().any(|b| b.contains("env_auth")));
    }

    #[test]
    fn temporary_session_credentials_are_refused() {
        let r = s3("[a]\ntype = s3\nprovider = AWS\nregion = us-east-1\naccess_key_id = ASIA1\nsecret_access_key = s\nsession_token = FwoGZXIvYXdz\n");
        assert!(!r.is_complete());
        assert!(
            r.blockers.iter().any(|b| b.contains("session_token")),
            "an expiring credential must not look importable: {:?}",
            r.blockers
        );
    }

    #[test]
    fn settings_driven_has_no_equivalent_for_are_reported_not_dropped_silently() {
        let r = s3("[a]\ntype = s3\nprovider = AWS\nregion = us-east-1\naccess_key_id = k\nsecret_access_key = s\nstorage_class = GLACIER\nacl = private\nserver_side_encryption = AES256\n");
        assert!(r.is_complete(), "none of these block an import");
        for key in ["storage_class", "acl", "server_side_encryption"] {
            assert!(
                r.notes.iter().any(|n| n.contains(key)),
                "{key} must be called out: {:?}",
                r.notes
            );
        }
    }

    #[test]
    fn no_note_or_blocker_ever_contains_a_secret() {
        // The whole importer reads a file full of credentials; every rendered
        // string must be safe to paste into a bug report.
        let r = s3("[a]\ntype = s3\nprovider = AWS\nregion = us-east-1\naccess_key_id = AKIAIOSFODNN7EXAMPLE\nsecret_access_key = wJalrXUtnFEMI-K7MDENG-bPxRfiCYEXAMPLEKEY\nsession_token = FwoGZXIvYXdzEXAMPLETOKEN\n");
        let rendered = format!("{:?} {:?} {:?}", r.notes, r.blockers, r);
        for secret in [
            "wJalrXUtnFEMI-K7MDENG-bPxRfiCYEXAMPLEKEY",
            "FwoGZXIvYXdzEXAMPLETOKEN",
        ] {
            assert!(!rendered.contains(secret), "leaked {secret}: {rendered}");
        }
        // The secret is still REACHABLE by an explicit caller.
        assert_eq!(
            r.secret_access_key.as_ref().map(Secret::expose),
            Some("wJalrXUtnFEMI-K7MDENG-bPxRfiCYEXAMPLEKEY")
        );
    }

    // -----------------------------------------------------------------
    // Drive
    // -----------------------------------------------------------------

    #[test]
    fn a_drive_remote_imports_its_folder_settings_and_never_its_token() {
        let d = drive(
            "[gd]\ntype = drive\nscope = drive\nroot_folder_id = 1AbCdEf\nteam_drive = 0ABCteam\ntoken = {\"access_token\":\"ya29.secret\",\"refresh_token\":\"1//0eSECRET\"}\n",
        );
        assert_eq!(d.root_folder_id.as_deref(), Some("1AbCdEf"));
        assert_eq!(d.team_drive.as_deref(), Some("0ABCteam"));
        assert_eq!(d.scope.as_deref(), Some("drive"));
        assert!(d.has_token, "the token is REPORTED as present");
        assert!(!d.blockers.is_empty(), "OAuth is always required");
        assert!(
            d.blockers.iter().any(|b| b.contains("RFC 6749")),
            "the reason must be the protocol rule, not a vague 'unsupported': {:?}",
            d.blockers
        );

        let rendered = format!("{:?} {:?} {:?}", d.notes, d.blockers, d);
        assert!(!rendered.contains("1//0eSECRET"), "leaked a refresh token");
        assert!(!rendered.contains("ya29.secret"), "leaked an access token");
    }

    #[test]
    fn a_byo_oauth_client_is_flagged_as_reusable_but_the_grant_is_not() {
        let d = drive("[gd]\ntype = drive\nclient_id = 123.apps.googleusercontent.com\nclient_secret = GOCSPX-mysecret\ntoken = {\"refresh_token\":\"1//0e\"}\n");
        assert!(d.has_byo_client);
        assert_eq!(d.client_id(), Some("123.apps.googleusercontent.com"));
        assert_eq!(
            d.client_secret().map(Secret::expose),
            Some("GOCSPX-mysecret")
        );
        assert!(
            d.notes.iter().any(|n| n.contains("reusable")),
            "the honest nuance must be stated: {:?}",
            d.notes
        );
        // ...and it does NOT stop being a blocker.
        assert!(d.blockers.iter().any(|b| b.contains("cannot be imported")));
        assert!(
            !format!("{:?} {:?} {:?}", d.notes, d.blockers, d).contains("GOCSPX-mysecret"),
            "the BYO client secret must not reach a rendered string either"
        );
    }

    #[test]
    fn a_service_account_drive_remote_cannot_be_reused_at_all() {
        let d = drive(
            "[gd]\ntype = drive\nservice_account_file = /home/u/sa.json\nroot_folder_id = 1X\n",
        );
        assert!(d.has_service_account);
        assert!(!d.has_token);
        assert!(d.blockers.iter().any(|b| b.contains("service_account")));
    }

    #[test]
    fn a_drive_remote_with_no_folder_settings_still_explains_what_to_do() {
        let d = drive("[gd]\ntype = drive\n");
        assert!(d.root_folder_id.is_none());
        assert!(d.team_drive.is_none());
        assert!(d
            .notes
            .iter()
            .any(|n| n.contains("pick a destination folder")));
        assert!(d.notes.iter().any(|n| n.contains("never authorized")));
    }

    // -----------------------------------------------------------------
    // Unsupported types
    // -----------------------------------------------------------------

    #[test]
    fn wrapper_backends_point_at_the_remote_they_wrap() {
        for (ty, inner) in [
            ("crypt", "remote"),
            ("alias", "remote"),
            ("chunker", "remote"),
            ("combine", "upstreams"),
            ("union", "upstreams"),
        ] {
            let out = classify(&remote(&format!("[w]\ntype = {ty}\n{inner} = other:\n")));
            match out {
                RemoteImport::Unsupported { reason, .. } => {
                    assert_eq!(
                        reason,
                        UnsupportedReason::Wrapper {
                            inner_option: Some(inner)
                        },
                        "{ty}"
                    );
                    assert!(reason.to_string().contains(inner));
                }
                _ => panic!("{ty} must not import"),
            }
        }
    }

    #[test]
    fn an_unrelated_backend_says_what_driven_supports() {
        let out = classify(&remote("[wd]\ntype = webdav\nhost = example.com\n"));
        assert!(out.summary().contains("not importable"));
        assert!(out.summary().contains("webdav"));
        match out {
            RemoteImport::Unsupported { reason, .. } => {
                assert_eq!(reason, UnsupportedReason::NoDrivenBackend);
                assert!(reason.to_string().contains("S3-compatible"));
                assert!(reason.to_string().contains("SSH (SFTP)"));
            }
            _ => panic!("webdav must not import"),
        }
    }

    #[test]
    fn a_section_with_no_type_is_reported_as_such() {
        let out = classify(&remote("[weird]\nfoo = bar\n"));
        match &out {
            RemoteImport::Unsupported {
                remote_type,
                reason,
                ..
            } => {
                assert!(remote_type.is_empty());
                assert_eq!(*reason, UnsupportedReason::MissingType);
            }
            _ => panic!("must not import"),
        }
        assert!(out.summary().contains("no type"));
    }

    #[test]
    fn the_type_is_matched_case_insensitively() {
        // `RcloneRemote::remote_type` lowercases, so a hand-edited `type = S3`
        // still lands on the S3 arm rather than falling through to unsupported.
        assert!(matches!(
            classify(&remote("[a]\ntype = S3\n")),
            RemoteImport::S3(_)
        ));
        assert!(matches!(
            classify(&remote("[a]\ntype = Drive\n")),
            RemoteImport::Drive(_)
        ));
    }

    #[test]
    fn backend_kind_and_name_are_reported_for_every_outcome() {
        use driven_remote::backend::BackendKind;
        let cases = [
            ("[a]\ntype = s3\n", Some(BackendKind::S3)),
            ("[a]\ntype = drive\n", Some(BackendKind::GoogleDrive)),
            ("[a]\ntype = sftp\n", Some(BackendKind::Sftp)),
            ("[a]\ntype = webdav\n", None),
        ];
        for (text, expected) in cases {
            let out = classify(&remote(text));
            assert_eq!(out.backend_kind(), expected, "{text}");
            assert_eq!(out.remote_name(), "a");
            assert!(!out.summary().is_empty());
        }
    }

    // -----------------------------------------------------------------
    // SFTP
    // -----------------------------------------------------------------

    fn sftp(text: &str) -> SftpRemote {
        match classify(&remote(text)) {
            RemoteImport::Sftp(r) => *r,
            other => panic!("expected an sftp import, got {}", other.summary()),
        }
    }

    #[test]
    fn an_sftp_remote_carries_across_host_port_and_user() {
        let r = sftp("[nas]\ntype = sftp\nhost = nas.example\nport = 2222\nuser = alice\n");
        assert_eq!(r.host.as_deref(), Some("nas.example"));
        assert_eq!(r.port, 2222);
        assert_eq!(r.user.as_deref(), Some("alice"));
    }

    #[test]
    fn an_sftp_remote_without_a_port_defaults_to_22() {
        let r = sftp("[nas]\ntype = sftp\nhost = nas.example\nuser = alice\n");
        assert_eq!(r.port, driven_sftp::DEFAULT_PORT);
        assert_eq!(driven_sftp::DEFAULT_PORT, 22);
    }

    #[test]
    fn an_unparsable_port_falls_back_to_the_default_with_a_note() {
        let r = sftp("[nas]\ntype = sftp\nhost = nas.example\nuser = alice\nport = not-a-port\n");
        assert_eq!(r.port, driven_sftp::DEFAULT_PORT);
        assert!(r.notes.iter().any(|n| n.contains("port")), "{:?}", r.notes);
    }

    #[test]
    fn a_missing_host_is_a_blocker() {
        let r = sftp("[nas]\ntype = sftp\nuser = alice\n");
        assert!(r.host.is_none());
        assert!(
            r.blockers.iter().any(|b| b.contains("host")),
            "{:?}",
            r.blockers
        );
    }

    #[test]
    fn a_missing_user_is_a_blocker() {
        let r = sftp("[nas]\ntype = sftp\nhost = nas.example\n");
        assert!(r.user.is_none());
        assert!(
            r.blockers.iter().any(|b| b.contains("user")),
            "{:?}",
            r.blockers
        );
    }

    #[test]
    fn the_password_is_reported_present_but_never_carried_across() {
        let r = sftp(
            "[nas]\ntype = sftp\nhost = nas.example\nuser = alice\npass = super-obscured-garbage\n",
        );
        assert!(r.has_password);
        assert!(
            r.notes.iter().any(|n| n.contains("obscured")),
            "{:?}",
            r.notes
        );
        assert!(
            r.blockers
                .iter()
                .any(|b| b.contains("re-enter your credential")),
            "{:?}",
            r.blockers
        );
        let rendered = format!("{r:?}");
        assert!(
            !rendered.contains("super-obscured-garbage"),
            "the obscured value must never be echoed back: {rendered}"
        );
    }

    #[test]
    fn a_private_key_file_path_is_noted_but_never_read() {
        let r = sftp(
            "[nas]\ntype = sftp\nhost = nas.example\nuser = alice\nkey_file = /home/u/.ssh/id_ed25519\n",
        );
        assert_eq!(r.key_file.as_deref(), Some("/home/u/.ssh/id_ed25519"));
        assert!(!r.has_password);
        assert!(
            r.notes
                .iter()
                .any(|n| n.contains("/home/u/.ssh/id_ed25519") && n.contains("PATH")),
            "{:?}",
            r.notes
        );
    }

    #[test]
    fn key_use_agent_is_noted_as_unsupported_in_v1() {
        let r =
            sftp("[nas]\ntype = sftp\nhost = nas.example\nuser = alice\nkey_use_agent = true\n");
        assert!(
            r.notes.iter().any(|n| n.contains("ssh-agent")),
            "{:?}",
            r.notes
        );
    }

    #[test]
    fn an_sftp_remote_with_nothing_credential_shaped_still_explains_what_to_do() {
        let r = sftp("[nas]\ntype = sftp\nhost = nas.example\nuser = alice\n");
        assert!(!r.has_password);
        assert!(r.key_file.is_none());
        assert!(
            r.notes
                .iter()
                .any(|n| n.contains("no password or private key")),
            "{:?}",
            r.notes
        );
    }

    #[test]
    fn an_sftp_remote_is_never_reported_as_a_one_step_import() {
        // Unlike S3, the credential ALWAYS has to be re-entered by hand, so
        // there is no state where an sftp remote imports fully.
        for text in [
            "[nas]\ntype = sftp\nhost = nas.example\nuser = alice\npass = x\n",
            "[nas]\ntype = sftp\n",
        ] {
            let r = sftp(text);
            assert!(!r.blockers.is_empty(), "{text}");
        }
    }

    #[test]
    fn no_sftp_note_or_blocker_ever_contains_the_password_value() {
        let r = sftp(
            "[nas]\ntype = sftp\nhost = nas.example\nuser = alice\npass = TOPSECRETOBSCURED\n",
        );
        let rendered = format!("{:?} {:?} {:?}", r.notes, r.blockers, r);
        assert!(!rendered.contains("TOPSECRETOBSCURED"), "{rendered}");
    }
}
