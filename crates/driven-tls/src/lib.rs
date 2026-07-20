//! `driven-tls` - the single shared home for Driven's custom-root-CA support
//! (issue #34 "Corporate CA pinning", DESIGN s5.8.7).
//!
//! Every outbound `reqwest` client in the workspace routes its builder through
//! [`apply_custom_ca`] (or, for the tauri-plugin-updater client whose hook is
//! infallible, [`load_certificates`]) so a corporate / TLS-inspection root CA
//! the user configures is trusted on ALL connections.
//!
//! This crate is a leaf: it depends only on `reqwest` + `thiserror`, so it can
//! sit below the three HTTP-using crates (`driven-net`, `driven-drive`,
//! `src-tauri`) without a dependency cycle. `driven-drive` is itself a leaf that
//! `driven-core` depends on, so no pre-existing crate could host this helper.
//!
//! # Trust semantics (locked - reviewed line-by-line)
//!
//! - **Additive.** The configured CA is *added* to reqwest's root store via
//!   [`reqwest::ClientBuilder::add_root_certificate`], which appends to the
//!   SAME `RootCertStore` that already holds the OS/enterprise native roots (the
//!   workspace reqwest feature is `rustls-tls-native-roots`). It never replaces
//!   them. Configuring a CA can only ADD trust.
//! - **No verification bypass.** This crate NEVER calls
//!   `tls_built_in_root_certs(false)`, `danger_accept_invalid_certs(true)`, or
//!   disables hostname verification. There is no code path here that weakens TLS
//!   validation.
//! - **Fail closed.** A configured-but-missing / unreadable / unparseable PEM,
//!   or a PEM that contains zero certificates, is an [`CaError`] that surfaces
//!   to the caller. We never silently fall back to system-trust-only once a CA
//!   has been configured.
//! - **`None` = unchanged default.** No configured path is a pure no-op (system
//!   trust only), leaving the builder exactly as it was.

use std::path::{Path, PathBuf};

use reqwest::{Certificate, ClientBuilder};

pub mod proxy;

pub use proxy::{
    apply_proxy, resolve_proxy, validate_manual_url, validate_pac_source, PacEngine, ProxyConfig,
    ProxyError, PROXY_MODES,
};

/// The resolved custom-root-CA setting: an optional path to a PEM file that may
/// contain one or more certificates. `None` (the default) means system trust
/// only - the unchanged behaviour.
#[derive(Clone, Debug, Default)]
pub struct CustomCaConfig {
    /// Path to the PEM bundle, or `None` for system-trust-only.
    path: Option<PathBuf>,
}

impl CustomCaConfig {
    /// System trust only (no custom CA). The default.
    #[must_use]
    pub fn none() -> Self {
        Self { path: None }
    }

    /// Build from an optional PEM path. The caller is expected to have already
    /// normalised an empty/whitespace string to `None`; a `Some(path)` here is
    /// treated as a real configured CA (and thus fail-closed at build time).
    #[must_use]
    pub fn from_path(path: Option<PathBuf>) -> Self {
        Self { path }
    }

    /// Whether a custom CA is configured.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.path.is_some()
    }

    /// The configured PEM path, if any.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

/// A failure loading or parsing the configured custom root CA. Every variant is
/// fail-closed: the caller must surface it, never proceed without the CA.
#[derive(Debug, thiserror::Error)]
pub enum CaError {
    /// The configured PEM file could not be read (missing / permissions / I/O).
    #[error("custom root CA file could not be read at {path}: {source}")]
    Read {
        /// The path that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The file was read but is not valid PEM certificate data.
    #[error("custom root CA file at {path} is not valid PEM: {message}")]
    Parse {
        /// The offending path.
        path: PathBuf,
        /// A human-readable parse detail (from reqwest).
        message: String,
    },
    /// The file was read and parsed but contained zero certificates. A file that
    /// trusts nothing is a configuration error, not a silent no-op.
    #[error("custom root CA file at {path} contained no certificates")]
    NoCertificates {
        /// The offending path.
        path: PathBuf,
    },
}

/// Parse ALL certificates from the configured PEM file.
///
/// - `None` config -> an empty `Vec` (the no-op default; the caller adds
///   nothing and keeps system trust only).
/// - `Some(path)` -> every certificate in the PEM bundle, or a fail-closed
///   [`CaError`] if the file is missing / unreadable / unparseable / empty.
///
/// This is the shared primitive behind both [`apply_custom_ca`] and the
/// updater's infallible `configure_client` hook (which needs the parsed certs
/// up front so the fallible load happens - and fails closed - before the
/// closure runs).
pub fn load_certificates(ca: &CustomCaConfig) -> Result<Vec<Certificate>, CaError> {
    match ca.path() {
        None => Ok(Vec::new()),
        Some(path) => load_certificates_from_path(path),
    }
}

/// Read + parse a PEM file into its certificates, fail-closed. Shared by
/// [`load_certificates`] and [`validate_ca_file`].
fn load_certificates_from_path(path: &Path) -> Result<Vec<Certificate>, CaError> {
    let pem = std::fs::read(path).map_err(|source| CaError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    // `from_pem_bundle` parses EVERY certificate in the bundle, eagerly decoding
    // each to DER (so a structurally invalid cert errors here rather than at
    // client-build time).
    let certs = Certificate::from_pem_bundle(&pem).map_err(|e| CaError::Parse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    // A file with no PEM CERTIFICATE blocks parses to an EMPTY vec (reqwest does
    // not treat that as an error). Fail closed: the user pointed us at a file
    // that trusts nothing, which is never what they meant.
    if certs.is_empty() {
        return Err(CaError::NoCertificates {
            path: path.to_path_buf(),
        });
    }
    Ok(certs)
}

/// Add the configured custom root CA (if any) to `builder`.
///
/// ADDITIVE - see the crate-level trust semantics. Returns the builder
/// unchanged when no CA is configured; fails closed if a configured CA cannot be
/// loaded. This is the single entry point every fallible reqwest build site in
/// the workspace calls.
pub fn apply_custom_ca(
    builder: ClientBuilder,
    ca: &CustomCaConfig,
) -> Result<ClientBuilder, CaError> {
    let mut builder = builder;
    for cert in load_certificates(ca)? {
        // `add_root_certificate` APPENDS to reqwest's root store, which already
        // holds the OS/enterprise native roots (feature rustls-tls-native-roots).
        // We never disable the built-in roots and never accept invalid certs -
        // this can only ADD trust.
        builder = builder.add_root_certificate(cert);
    }
    Ok(builder)
}

/// Validate a candidate CA file for the settings UI: returns the number of
/// certificates it contains, or a descriptive read/parse error. Uses the exact
/// same fail-closed rules as the client-build path, so "valid at save" implies
/// "will not fail-closed at build" (as long as the file is still there).
pub fn validate_ca_file(path: &Path) -> Result<usize, CaError> {
    Ok(load_certificates_from_path(path)?.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // Two distinct self-signed test CAs (RSA-2048, CN "Driven Test CA 1/2").
    // Embedded as constants so the unit tests need no runtime cert generation
    // and no extra dev-dependency.
    const TEST_CA_1: &str = "-----BEGIN CERTIFICATE-----\n\
MIIDFzCCAf+gAwIBAgIUB5Q41gPo/wu/gcL39WRKnSuXLUYwDQYJKoZIhvcNAQEL\n\
BQAwGzEZMBcGA1UEAwwQRHJpdmVuIFRlc3QgQ0EgMTAeFw0yNjA3MjAxNTQ4NTFa\n\
Fw0zNjA3MTcxNTQ4NTFaMBsxGTAXBgNVBAMMEERyaXZlbiBUZXN0IENBIDEwggEi\n\
MA0GCSqGSIb3DQEBAQUAA4IBDwAwggEKAoIBAQDwFFtyR6a9TV01KCQVU68OlKGf\n\
YRiXaY+YWc6q0jql65FD7934nEBPNXaDEc/zsxUWqsioyW81gzgbK/RrE98cgSQC\n\
tm5fsMPvL8H6nhKQHMuJwBgo4LawGsLqZR2uvICTOPDFw3f7J+/INgNDpJQ+LgOb\n\
QqQtjcyHRFcRqhoWspOAdmc5NGKQ5eZxIAxvdK6P5wzbXUoW5xPi6TOLWeuQAn90\n\
Bai+mZ0TfnxMauvfC5Mf96K9Y/CRkulRqnddT1KVbmeMhv2ilcOd20rVRu5mq9tb\n\
FHmFfsCnbxs0JZA3OC0Fd6lCGgXR4yXxQZWH97WAzZOWVzYE9igGRZ/S38U9AgMB\n\
AAGjUzBRMB0GA1UdDgQWBBR/xbCt2uzNY9bEXNd4nydqypUveDAfBgNVHSMEGDAW\n\
gBR/xbCt2uzNY9bEXNd4nydqypUveDAPBgNVHRMBAf8EBTADAQH/MA0GCSqGSIb3\n\
DQEBCwUAA4IBAQAK1E2Kewr22T/UvhppVdzEtzHFMi4psji31MlA2PfRVR5vhUFz\n\
rAaZIBjG7E/3i+LeEKXJd6MZZ6+e0HFo+IGHSEMCLi9DvA+uAQhBflFI8uDBX8rb\n\
ewjWzBB4j9JElIuVvUUlhzuWV9DfMGwWyX+8lpnVmpU5vjbb4C0/uSelu6EdoMYE\n\
diyL/TNANqgBb+0vuAdO8ua5FPMjerNyIUSZSli9xxaHv82XJC+poD11nwBo8Tsh\n\
s5w3VBWjhX/HCnoyVqioMbagxiBz4FzWoJPQjNnDb5LlMmFzGrHSekuem1D9Ol2P\n\
TcSAr7WHM8cnvHrbKpGrZGfuL9wI7cnaDPSd\n\
-----END CERTIFICATE-----\n";

    const TEST_CA_2: &str = "-----BEGIN CERTIFICATE-----\n\
MIIDFzCCAf+gAwIBAgIUCVrtj+PWlOUKN/L2FVgXZMeFrg0wDQYJKoZIhvcNAQEL\n\
BQAwGzEZMBcGA1UEAwwQRHJpdmVuIFRlc3QgQ0EgMjAeFw0yNjA3MjAxNTQ4NTJa\n\
Fw0zNjA3MTcxNTQ4NTJaMBsxGTAXBgNVBAMMEERyaXZlbiBUZXN0IENBIDIwggEi\n\
MA0GCSqGSIb3DQEBAQUAA4IBDwAwggEKAoIBAQC5DFoP1IBAxHiU672yun7uirVe\n\
4PqIZVNge/tuw6sNZV0plVbJeFT3msCXc8j0V4TtULvIHqg4156yF/tEgq5c/Reu\n\
tEV97SsRqYic1Tr64OdSESN93Rn6vdm/MnNtuTMYLhrxiRSO4TAZdtRIG+GU239m\n\
aRYJCbzTCKJ0k+nQSKeCaeQtkZJDdl7bukJ6t+Hr9yHqLTpb/PqLeFh1l1sdgsSt\n\
xOT3BreyFMW4zA4luPHCul0pA+vtg0TiNZAN2vYWlQmetc01KjOhBApM/90xNeI0\n\
TbJ59I003kYk7Ia/OL0gJXMroLuN4+Ccio9MH9ZZwn2bUygnGLKEVSrJiIDjAgMB\n\
AAGjUzBRMB0GA1UdDgQWBBTQP5XWMFVFELIDoAq3d2zjbfCjgzAfBgNVHSMEGDAW\n\
gBTQP5XWMFVFELIDoAq3d2zjbfCjgzAPBgNVHRMBAf8EBTADAQH/MA0GCSqGSIb3\n\
DQEBCwUAA4IBAQAY0L1I/MLtYxrvRK13OXRRQjVmq54BgrmTBjimHPyHlrKURNFd\n\
5ZaVpHCfGWPKcGbzYcQ8WOG5v5IjZKunJ0AV/i9HuDtpmLohlsC9Mng79VTxa5ME\n\
zku1eTgnVzwEruJpETbaYch2M+7+QSua6xqYRJEkpsRfvZ/rJf2l7me/+FHmvRQ3\n\
7g8mGNMQNAZn90eQjubV+sLMJehm/eoA1N2+5NTXBh7O1PauPW6SfxcDv2C12HKn\n\
ve0oTKIhVkYpJ9M0/rQvVaTst7M4XQSCtxXxVGFVAyRXTmKbMIfaByjPG2euSC1O\n\
YkOpjW4wgL2uWUUbn7AOlUwyBmMQcem7nCnE\n\
-----END CERTIFICATE-----\n";

    /// Write `content` to a uniquely-named temp file and return its path. The
    /// file lives until the process exits (fine for a unit test); a unique name
    /// avoids cross-test collisions without pulling in a tempfile dep.
    fn write_temp(name: &str, content: &[u8]) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("driven-tls-test-{}-{name}", std::process::id()));
        let mut f = std::fs::File::create(&path).expect("create temp file");
        f.write_all(content).expect("write temp file");
        path
    }

    #[test]
    fn none_config_is_a_noop() {
        let ca = CustomCaConfig::none();
        assert!(!ca.is_enabled());
        assert!(load_certificates(&ca).expect("none loads").is_empty());
        // The builder must still build (system trust only).
        let built = apply_custom_ca(reqwest::Client::builder(), &ca)
            .expect("no-op apply")
            .build();
        assert!(built.is_ok(), "no-CA client should build: {built:?}");
    }

    #[test]
    fn single_cert_parses_and_applies() {
        let path = write_temp("single.pem", TEST_CA_1.as_bytes());
        let ca = CustomCaConfig::from_path(Some(path.clone()));
        assert!(ca.is_enabled());

        assert_eq!(validate_ca_file(&path).expect("validate single"), 1);
        assert_eq!(load_certificates(&ca).expect("load single").len(), 1);
        let built = apply_custom_ca(reqwest::Client::builder(), &ca)
            .expect("apply single")
            .build();
        assert!(built.is_ok(), "single-CA client should build: {built:?}");
    }

    #[test]
    fn multi_cert_bundle_parses_all() {
        let bundle = format!("{TEST_CA_1}{TEST_CA_2}");
        let path = write_temp("bundle.pem", bundle.as_bytes());
        let ca = CustomCaConfig::from_path(Some(path.clone()));

        assert_eq!(validate_ca_file(&path).expect("validate bundle"), 2);
        assert_eq!(load_certificates(&ca).expect("load bundle").len(), 2);
        let built = apply_custom_ca(reqwest::Client::builder(), &ca)
            .expect("apply bundle")
            .build();
        assert!(built.is_ok(), "bundle client should build: {built:?}");
    }

    #[test]
    fn garbage_file_fails_closed_no_certificates() {
        let path = write_temp("garbage.pem", b"this is not a certificate\n");
        let ca = CustomCaConfig::from_path(Some(path.clone()));

        // A file with no PEM CERTIFICATE blocks parses to an empty set -> error.
        let err = load_certificates(&ca).expect_err("garbage must fail");
        assert!(matches!(err, CaError::NoCertificates { .. }), "got {err:?}");
        assert!(validate_ca_file(&path).is_err());
        // And the client build must fail closed, not silently succeed.
        assert!(apply_custom_ca(reqwest::Client::builder(), &ca).is_err());
    }

    #[test]
    fn missing_file_fails_closed_read_error() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "driven-tls-does-not-exist-{}.pem",
            std::process::id()
        ));
        let ca = CustomCaConfig::from_path(Some(path.clone()));

        let err = load_certificates(&ca).expect_err("missing must fail");
        assert!(matches!(err, CaError::Read { .. }), "got {err:?}");
        assert!(validate_ca_file(&path).is_err());
        assert!(apply_custom_ca(reqwest::Client::builder(), &ca).is_err());
    }

    #[test]
    fn truncated_pem_body_fails_closed() {
        // A well-formed PEM header/footer but corrupt base64 body must not be
        // accepted (fail closed via a parse error, not a silent empty set).
        let broken =
            "-----BEGIN CERTIFICATE-----\nnot-valid-base64!!!\n-----END CERTIFICATE-----\n";
        let path = write_temp("broken.pem", broken.as_bytes());
        let ca = CustomCaConfig::from_path(Some(path));
        assert!(
            load_certificates(&ca).is_err(),
            "corrupt PEM body must fail closed"
        );
    }
}
