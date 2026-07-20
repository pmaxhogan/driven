//! Proxy configuration for every outbound `reqwest` client (issue #34, DESIGN
//! s5.8.7). Threaded through the SAME build sites as [`crate::CustomCaConfig`].
//!
//! # Modes (locked)
//!
//! - **System** (default): honour `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` env vars
//!   via reqwest's built-in support - the unchanged V1 behaviour. We add neither
//!   `.proxy()` nor `.no_proxy()`, so reqwest's automatic env-proxy pickup stays
//!   on.
//! - **None**: explicit `.no_proxy()` - bypass every proxy, including the env
//!   vars, for a direct connection.
//! - **Manual**: a single proxy URL (`http://`, `https://`, `socks5://` or
//!   `socks5h://`) applied to all schemes via [`reqwest::Proxy::all`]. `socks5h`
//!   does proxy-side DNS. Adding an explicit `.proxy()` overrides reqwest's env
//!   pickup, so a configured manual proxy always wins over `HTTP_PROXY`.
//! - **Pac**: a PAC (proxy auto-config) file - a URL or local path to a script
//!   defining `FindProxyForURL(url, host)`. The script is fetched/read + compiled
//!   once, then evaluated per-URL via [`reqwest::Proxy::custom`].
//!
//! # Fail-closed philosophy (mirrors [`crate::apply_custom_ca`])
//!
//! A configured-but-invalid proxy is an [`ProxyError`] that surfaces to the
//! caller at BOTH settings-save and client-build time - a bad manual URL, an
//! unreachable/unparseable PAC file, or a PAC script that does not compile is
//! NEVER silently downgraded to a direct/unproxied connection. Only a corrupt
//! *mode* string (which settings validation prevents) falls back to `System`.

use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs, UdpSocket};
use std::sync::{Arc, LazyLock, Mutex};

use boa_engine::{
    js_string, native_function::NativeFunction, property::Attribute, Context, JsArgs, JsResult,
    JsValue, Source,
};
use lru::LruCache;
use reqwest::ClientBuilder;
use url::Url;

const TARGET: &str = "driven_tls::proxy";

/// User-agent for the PAC-file fetch (kept distinct so a proxy log can tell the
/// auto-config fetch apart from Driven's data traffic).
const PAC_FETCH_USER_AGENT: &str = concat!("Driven/", env!("CARGO_PKG_VERSION"), " (pac-fetch)");

/// The valid `proxy_mode` strings (settings validation + docs). Kept here so the
/// settings layer and this crate agree on the enum.
pub const PROXY_MODES: &[&str] = &["system", "none", "manual", "pac"];

/// Proxy URL schemes we accept in manual mode. `socks5h` routes DNS through the
/// proxy (the `h` = hostname resolution proxy-side); both require reqwest's
/// `socks` feature (enabled workspace-wide for issue #34).
const SUPPORTED_MANUAL_SCHEMES: &[&str] = &["http", "https", "socks5", "socks5h"];

/// How many distinct hosts' PAC decisions to cache before evicting the least
/// recently used. Bounds how often the JS engine actually runs (at most once per
/// unique host until eviction).
const PAC_CACHE_CAPACITY: usize = 256;

/// A failure resolving or applying the configured proxy. Every variant is
/// fail-closed: the caller surfaces it, never proceeds unproxied.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    /// The manual proxy URL could not be parsed.
    #[error("proxy URL `{url}` is not valid: {message}")]
    InvalidUrl {
        /// The offending URL.
        url: String,
        /// Parse detail.
        message: String,
    },
    /// The manual proxy URL used a scheme we do not support.
    #[error(
        "proxy URL `{url}` uses unsupported scheme `{scheme}` \
         (expected http, https, socks5, or socks5h)"
    )]
    UnsupportedScheme {
        /// The offending URL.
        url: String,
        /// The rejected scheme.
        scheme: String,
    },
    /// Manual mode selected but no URL configured.
    #[error("manual proxy mode requires a proxy URL, but none was configured")]
    MissingManualUrl,
    /// PAC mode selected but no PAC source configured.
    #[error("PAC proxy mode requires a PAC file URL or path, but none was configured")]
    MissingPacSource,
    /// The PAC file could not be read from a local path. (`location` is not
    /// named `source` because thiserror reserves that field name for a nested
    /// `std::error::Error` source.)
    #[error("PAC file could not be read from `{location}`: {message}")]
    PacRead {
        /// The path/URL we tried.
        location: String,
        /// I/O detail.
        message: String,
    },
    /// The PAC file could not be fetched over HTTP(S).
    #[error("PAC file could not be fetched from `{location}`: {message}")]
    PacFetch {
        /// The URL we tried.
        location: String,
        /// Transport / status detail.
        message: String,
    },
    /// The PAC file was read/fetched but was empty.
    #[error("PAC file at `{location}` was empty")]
    PacEmpty {
        /// The source that was empty.
        location: String,
    },
    /// The PAC script did not compile or does not define `FindProxyForURL`.
    #[error("PAC script from `{location}` is not usable: {message}")]
    PacCompile {
        /// The source that failed.
        location: String,
        /// Compile / definition detail.
        message: String,
    },
}

/// The resolved runtime proxy setting, ready to apply to a [`ClientBuilder`].
/// Built via [`resolve_proxy`] from the persisted `(mode, value)` pair.
#[derive(Clone, Default)]
pub enum ProxyConfig {
    /// Honour the `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` env vars (default).
    #[default]
    System,
    /// Explicitly bypass all proxies (`.no_proxy()`).
    None,
    /// A single manual proxy URL applied to every scheme.
    Manual(String),
    /// A compiled PAC script, evaluated per-URL. `Arc` so all clients built from
    /// one resolution share the same warm per-host decision cache.
    Pac(Arc<PacEngine>),
}

impl std::fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::System => write!(f, "ProxyConfig::System"),
            Self::None => write!(f, "ProxyConfig::None"),
            // Redact the URL: a proxy URL can carry `user:pass@` credentials.
            Self::Manual(_) => write!(f, "ProxyConfig::Manual(<redacted>)"),
            Self::Pac(e) => write!(f, "ProxyConfig::Pac({})", e.source),
        }
    }
}

impl ProxyConfig {
    /// System trust default (env-proxy honoured). Mirrors
    /// [`crate::CustomCaConfig::none`] as the explicit no-op constructor.
    #[must_use]
    pub fn system() -> Self {
        Self::System
    }

    /// The manual proxy URL, if this is [`ProxyConfig::Manual`]. Exposed so the
    /// updater's reqwest-0.13 bridge can re-apply the same proxy.
    #[must_use]
    pub fn manual_url(&self) -> Option<&str> {
        match self {
            Self::Manual(url) => Some(url),
            _ => None,
        }
    }

    /// The compiled PAC engine, if this is [`ProxyConfig::Pac`]. Exposed so the
    /// updater's reqwest-0.13 bridge can drive the same per-URL evaluation.
    #[must_use]
    pub fn pac_engine(&self) -> Option<Arc<PacEngine>> {
        match self {
            Self::Pac(engine) => Some(Arc::clone(engine)),
            _ => None,
        }
    }

    /// Whether this config explicitly bypasses all proxies ([`ProxyConfig::None`]).
    #[must_use]
    pub fn is_no_proxy(&self) -> bool {
        matches!(self, Self::None)
    }
}

/// Validate a manual proxy URL: parseable, a supported scheme, and a non-empty
/// host. Shared by the settings-save path, the IPC validate command, and
/// [`apply_proxy`], so "valid at save" implies "will not fail-closed at build".
pub fn validate_manual_url(raw: &str) -> Result<(), ProxyError> {
    let parsed = Url::parse(raw).map_err(|e| ProxyError::InvalidUrl {
        url: raw.to_string(),
        message: e.to_string(),
    })?;
    let scheme = parsed.scheme();
    if !SUPPORTED_MANUAL_SCHEMES.contains(&scheme) {
        return Err(ProxyError::UnsupportedScheme {
            url: raw.to_string(),
            scheme: scheme.to_string(),
        });
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err(ProxyError::InvalidUrl {
            url: raw.to_string(),
            message: "proxy URL has no host".to_string(),
        });
    }
    Ok(())
}

/// Apply the resolved proxy to `builder` (fail-closed). The single entry point
/// every reqwest build site in the workspace calls, mirroring
/// [`crate::apply_custom_ca`].
///
/// - `System` -> builder unchanged (reqwest's env-proxy pickup stays on).
/// - `None` -> `.no_proxy()`.
/// - `Manual(url)` -> `reqwest::Proxy::all(url)` on all schemes; a bad URL is an
///   `Err` (the manual URL is validated again here so a client build can never
///   silently drop a configured proxy).
/// - `Pac(engine)` -> a per-URL [`reqwest::Proxy::custom`] closure driving the
///   compiled PAC script.
pub fn apply_proxy(
    builder: ClientBuilder,
    proxy: &ProxyConfig,
) -> Result<ClientBuilder, ProxyError> {
    match proxy {
        ProxyConfig::System => Ok(builder),
        ProxyConfig::None => Ok(builder.no_proxy()),
        ProxyConfig::Manual(url) => {
            // Re-validate at build time: a persisted-then-corrupted URL fails
            // closed rather than building an unproxied client.
            validate_manual_url(url)?;
            let proxy = reqwest::Proxy::all(url).map_err(|e| ProxyError::InvalidUrl {
                url: url.clone(),
                message: e.to_string(),
            })?;
            Ok(builder.proxy(proxy))
        }
        ProxyConfig::Pac(engine) => {
            let engine = Arc::clone(engine);
            // reqwest requires the closure be Fn + Send + Sync + 'static. The
            // engine is Arc<Send+Sync>; a fresh boa Context is built INSIDE
            // `evaluate_str` per call and never crosses the closure boundary.
            let proxy = reqwest::Proxy::custom(move |url| {
                engine
                    .evaluate_str(url.as_str())
                    .and_then(|proxy_url| reqwest::Url::parse(&proxy_url).ok())
            });
            Ok(builder.proxy(proxy))
        }
    }
}

/// Resolve a persisted `(mode, value)` proxy setting into a runtime
/// [`ProxyConfig`], fetching + compiling the PAC script for PAC mode. Async
/// because PAC mode may fetch the script over HTTP. Fail-closed for a configured
/// manual URL / PAC source; an unknown `mode` degrades to `System` (the corrupt-
/// settings default, matching [`crate::CustomCaConfig`]'s blob-read degradation).
///
/// `ca` is applied to the PAC-fetch client so a PAC file served over an
/// internal TLS-inspected host is trusted the same way as data traffic. The PAC
/// fetch itself is never proxied (a PAC file cannot be behind the proxy it
/// selects).
pub async fn resolve_proxy(
    mode: &str,
    value: Option<&str>,
    ca: &crate::CustomCaConfig,
) -> Result<ProxyConfig, ProxyError> {
    let trimmed = value.map(str::trim).filter(|v| !v.is_empty());
    match mode {
        "none" => Ok(ProxyConfig::None),
        "manual" => {
            let url = trimmed.ok_or(ProxyError::MissingManualUrl)?;
            validate_manual_url(url)?;
            Ok(ProxyConfig::Manual(url.to_string()))
        }
        "pac" => {
            let source = trimmed.ok_or(ProxyError::MissingPacSource)?;
            let engine = load_pac_engine(source, ca).await?;
            Ok(ProxyConfig::Pac(engine))
        }
        // "system" and any unrecognised (corrupt) value: env-proxy default.
        _ => Ok(ProxyConfig::System),
    }
}

/// Validate a PAC source for the settings UI / save path: fetch/read + compile
/// it (fail-closed), returning nothing on success. Async (may fetch). Uses the
/// exact same path as [`resolve_proxy`], so "valid here" implies "resolves at
/// build". Bypasses the process cache so a re-validate always re-checks the
/// live source.
pub async fn validate_pac_source(
    source: &str,
    ca: &crate::CustomCaConfig,
) -> Result<(), ProxyError> {
    let source = source.trim();
    if source.is_empty() {
        return Err(ProxyError::MissingPacSource);
    }
    let script = fetch_pac_script(source, ca).await?;
    PacEngine::compile(script, source.to_string()).map(|_| ())
}

// ---------------------------------------------------------------------------
// PAC fetch + process-global compiled-engine cache
// ---------------------------------------------------------------------------

/// A compiled PAC engine cached against the exact source string it came from, so
/// the ~9 client-build load sites reuse ONE fetch + ONE warm decision cache
/// instead of refetching per operation.
struct CachedPac {
    source: String,
    engine: Arc<PacEngine>,
}

static PAC_CACHE: LazyLock<Mutex<Option<CachedPac>>> = LazyLock::new(|| Mutex::new(None));

/// Return the compiled engine for `source`, reusing the process cache when the
/// source is unchanged; otherwise fetch + compile once and cache it.
async fn load_pac_engine(
    source: &str,
    ca: &crate::CustomCaConfig,
) -> Result<Arc<PacEngine>, ProxyError> {
    // Fast path: cache hit. Drop the lock BEFORE any await.
    if let Some(hit) = PAC_CACHE
        .lock()
        .expect("PAC cache mutex poisoned")
        .as_ref()
        .filter(|c| c.source == source)
        .map(|c| Arc::clone(&c.engine))
    {
        return Ok(hit);
    }

    let script = fetch_pac_script(source, ca).await?;
    let engine = Arc::new(PacEngine::compile(script, source.to_string())?);

    // Store (last writer wins if two sites raced a miss - harmless, idempotent).
    *PAC_CACHE.lock().expect("PAC cache mutex poisoned") = Some(CachedPac {
        source: source.to_string(),
        engine: Arc::clone(&engine),
    });
    Ok(engine)
}

/// Fetch (URL) or read (local path / `file://`) the raw PAC script text.
async fn fetch_pac_script(source: &str, ca: &crate::CustomCaConfig) -> Result<String, ProxyError> {
    let script = if let Some(url) = as_http_url(source) {
        // A basic client with the user's custom CA applied (fail-closed) and NO
        // proxy - the PAC file cannot itself be behind the proxy it chooses.
        let builder = reqwest::Client::builder()
            .user_agent(PAC_FETCH_USER_AGENT)
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(30));
        let client = crate::apply_custom_ca(builder, ca)
            .map_err(|e| ProxyError::PacFetch {
                location: source.to_string(),
                message: format!("custom CA could not be applied to the PAC fetch: {e}"),
            })?
            .build()
            .map_err(|e| ProxyError::PacFetch {
                location: source.to_string(),
                message: format!("could not build the PAC fetch client: {e}"),
            })?;
        let resp = client
            .get(url)
            .send()
            .await
            .map_err(|e| ProxyError::PacFetch {
                location: source.to_string(),
                message: e.to_string(),
            })?;
        let resp = resp.error_for_status().map_err(|e| ProxyError::PacFetch {
            location: source.to_string(),
            message: e.to_string(),
        })?;
        resp.text().await.map_err(|e| ProxyError::PacFetch {
            location: source.to_string(),
            message: e.to_string(),
        })?
    } else {
        // Local path (bare path or `file://`).
        let path = file_url_to_path(source);
        std::fs::read_to_string(&path).map_err(|e| ProxyError::PacRead {
            location: source.to_string(),
            message: e.to_string(),
        })?
    };
    if script.trim().is_empty() {
        return Err(ProxyError::PacEmpty {
            location: source.to_string(),
        });
    }
    Ok(script)
}

/// Interpret `source` as an `http(s)` URL, or `None` if it is a local path.
fn as_http_url(source: &str) -> Option<Url> {
    let url = Url::parse(source).ok()?;
    matches!(url.scheme(), "http" | "https").then_some(url)
}

/// Map a `file://` URL to a filesystem path; a bare path passes through.
fn file_url_to_path(source: &str) -> std::path::PathBuf {
    if let Ok(url) = Url::parse(source) {
        if url.scheme() == "file" {
            if let Ok(path) = url.to_file_path() {
                return path;
            }
        }
    }
    std::path::PathBuf::from(source)
}

// ---------------------------------------------------------------------------
// PacEngine - compiled PAC script + per-host LRU decision cache
// ---------------------------------------------------------------------------

/// A compiled PAC script evaluated per-URL. `Send + Sync`: the script is an
/// `Arc<str>` and the cache a `Mutex<LruCache>`; a boa `Context` (which is
/// `!Send`) is created fresh inside [`Self::run`] on whatever thread calls it and
/// never stored, so the engine can live in a `reqwest::Proxy::custom` closure.
pub struct PacEngine {
    script: Arc<str>,
    source: String,
    /// host -> chosen proxy URL (`None` = DIRECT). Bounds JS-engine invocations
    /// to at most once per unique host until eviction.
    cache: Mutex<LruCache<String, Option<String>>>,
}

impl PacEngine {
    /// Compile + validate a PAC script (checks it parses and defines
    /// `FindProxyForURL`). Does NOT evaluate it against any URL (so no DNS at
    /// compile time). Fail-closed on any error.
    pub fn compile(script: String, source: String) -> Result<Self, ProxyError> {
        if script.trim().is_empty() {
            return Err(ProxyError::PacEmpty { location: source });
        }
        // Compile-time check in a throwaway context: the script loads and
        // `FindProxyForURL` is a function. The per-eval contexts are separate.
        let mut ctx = Context::default();
        register_pac_helpers(&mut ctx).map_err(|e| ProxyError::PacCompile {
            location: source.clone(),
            message: format!("could not install PAC helpers: {e}"),
        })?;
        ctx.eval(Source::from_bytes(script.as_bytes()))
            .map_err(|e| ProxyError::PacCompile {
                location: source.clone(),
                message: e.to_string(),
            })?;
        let is_fn = ctx
            .eval(Source::from_bytes(b"typeof FindProxyForURL === 'function'"))
            .map_err(|e| ProxyError::PacCompile {
                location: source.clone(),
                message: e.to_string(),
            })?;
        if !is_fn.as_boolean().unwrap_or(false) {
            return Err(ProxyError::PacCompile {
                location: source,
                message: "script does not define a FindProxyForURL(url, host) function".to_string(),
            });
        }
        Ok(Self {
            script: Arc::from(script.into_boxed_str()),
            source,
            cache: Mutex::new(LruCache::new(
                std::num::NonZeroUsize::new(PAC_CACHE_CAPACITY).expect("capacity is non-zero"),
            )),
        })
    }

    /// The source string this engine was compiled from (URL / path). For logs.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Evaluate the PAC script for `url_str`, returning the chosen proxy URL
    /// (`http://`, `https://`, or `socks5://...`) or `None` for a direct
    /// connection. Version-neutral (`&str` in, `Option<String>` out) so both
    /// reqwest 0.12 and the updater's reqwest 0.13 can drive it.
    ///
    /// Cached per host. On a script runtime error we log and return `None`
    /// (direct), matching browser PAC-failure behaviour - the script itself was
    /// validated at compile time, so this only covers per-request edge failures
    /// (e.g. a DNS timeout inside `isInNet`).
    #[must_use]
    pub fn evaluate_str(&self, url_str: &str) -> Option<String> {
        let host = host_of(url_str)?;
        if let Some(cached) = self
            .cache
            .lock()
            .expect("PAC decision cache poisoned")
            .get(&host)
            .cloned()
        {
            return cached;
        }
        let decision = match self.run(url_str, &host) {
            Ok(decision) => decision,
            Err(message) => {
                tracing::warn!(
                    target: TARGET,
                    host = %host,
                    source = %self.source,
                    %message,
                    "PAC evaluation failed at request time; falling back to a direct connection"
                );
                None
            }
        };
        self.cache
            .lock()
            .expect("PAC decision cache poisoned")
            .put(host, decision.clone());
        decision
    }

    /// Run the compiled script for one `(url, host)` in a fresh boa context.
    /// Returns the chosen proxy URL (or `None` for DIRECT). `Err(String)` on any
    /// engine error.
    fn run(&self, url: &str, host: &str) -> Result<Option<String>, String> {
        let mut ctx = Context::default();
        register_pac_helpers(&mut ctx).map_err(|e| e.to_string())?;
        ctx.eval(Source::from_bytes(self.script.as_bytes()))
            .map_err(|e| e.to_string())?;
        // Inject url/host as globals (no string interpolation -> no injection).
        ctx.register_global_property(
            js_string!("__driven_pac_url"),
            js_string!(url),
            Attribute::all(),
        )
        .map_err(|e| e.to_string())?;
        ctx.register_global_property(
            js_string!("__driven_pac_host"),
            js_string!(host),
            Attribute::all(),
        )
        .map_err(|e| e.to_string())?;
        let result = ctx
            .eval(Source::from_bytes(
                b"String(FindProxyForURL(__driven_pac_url, __driven_pac_host))",
            ))
            .map_err(|e| e.to_string())?;
        let directive = result
            .to_string(&mut ctx)
            .map_err(|e| e.to_string())?
            .to_std_string_escaped();
        Ok(parse_pac_result(&directive))
    }
}

/// Extract the host from a request URL for the per-host cache key.
fn host_of(url_str: &str) -> Option<String> {
    Url::parse(url_str)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
}

/// Parse a `FindProxyForURL` return value into the first usable proxy URL, or
/// `None` for DIRECT. The return is a `;`-separated preference list of
/// directives (`DIRECT`, `PROXY host:port`, `HTTPS host:port`,
/// `SOCKS/SOCKS5 host:port`). reqwest's custom-proxy closure can express only a
/// single proxy-or-direct, so we take the FIRST directive: `DIRECT` first =>
/// direct; a proxy first => that proxy (fallback chains are not expressible).
fn parse_pac_result(result: &str) -> Option<String> {
    for directive in result.split(';') {
        let directive = directive.trim();
        if directive.is_empty() {
            continue;
        }
        let mut parts = directive.split_whitespace();
        let kind = parts.next()?.to_ascii_uppercase();
        if kind == "DIRECT" {
            return None;
        }
        let Some(hostport) = parts.next() else {
            continue;
        };
        let scheme = match kind.as_str() {
            "PROXY" | "HTTP" => "http",
            "HTTPS" => "https",
            // reqwest supports only SOCKS5; map SOCKS/SOCKS5 to socks5. (SOCKS
            // without a version is historically SOCKS4, which reqwest cannot do;
            // socks5 is the pragmatic best-effort.)
            "SOCKS" | "SOCKS5" => "socks5",
            _ => continue, // unknown directive - try the next one
        };
        return Some(format!("{scheme}://{hostport}"));
    }
    // An empty / all-unknown result string: treat as DIRECT.
    None
}

// ---------------------------------------------------------------------------
// PAC helper functions (registered into every eval context)
// ---------------------------------------------------------------------------

/// Register the standard PAC helper functions on `ctx`. Implements the commonly
/// used subset; the date/time predicates (`weekdayRange`/`dateRange`/`timeRange`)
/// are defined-but-conservative (always `false`) so a script that calls them does
/// not throw - it simply does not take a time-gated branch.
fn register_pac_helpers(ctx: &mut Context) -> JsResult<()> {
    fn arg_str(args: &[JsValue], i: usize, ctx: &mut Context) -> String {
        args.get_or_undefined(i)
            .to_string(ctx)
            .map(|s| s.to_std_string_escaped())
            .unwrap_or_default()
    }

    ctx.register_global_callable(
        js_string!("isPlainHostName"),
        1,
        NativeFunction::from_fn_ptr(|_, args, ctx| {
            let host = arg_str(args, 0, ctx);
            Ok(JsValue::from(!host.contains('.')))
        }),
    )?;

    ctx.register_global_callable(
        js_string!("dnsDomainIs"),
        2,
        NativeFunction::from_fn_ptr(|_, args, ctx| {
            let host = arg_str(args, 0, ctx).to_ascii_lowercase();
            let domain = arg_str(args, 1, ctx).to_ascii_lowercase();
            Ok(JsValue::from(host.ends_with(&domain)))
        }),
    )?;

    ctx.register_global_callable(
        js_string!("localHostOrDomainIs"),
        2,
        NativeFunction::from_fn_ptr(|_, args, ctx| {
            let host = arg_str(args, 0, ctx).to_ascii_lowercase();
            let hostdom = arg_str(args, 1, ctx).to_ascii_lowercase();
            // Exact match, or a plain hostname that is the leading label of the
            // fully-qualified name.
            let matched = host == hostdom
                || (!host.contains('.') && hostdom.starts_with(&format!("{host}.")));
            Ok(JsValue::from(matched))
        }),
    )?;

    ctx.register_global_callable(
        js_string!("dnsDomainLevels"),
        1,
        NativeFunction::from_fn_ptr(|_, args, ctx| {
            let host = arg_str(args, 0, ctx);
            Ok(JsValue::from(host.matches('.').count() as i32))
        }),
    )?;

    ctx.register_global_callable(
        js_string!("shExpMatch"),
        2,
        NativeFunction::from_fn_ptr(|_, args, ctx| {
            let text = arg_str(args, 0, ctx);
            let pattern = arg_str(args, 1, ctx);
            Ok(JsValue::from(glob_match(&pattern, &text)))
        }),
    )?;

    ctx.register_global_callable(
        js_string!("dnsResolve"),
        1,
        NativeFunction::from_fn_ptr(|_, args, ctx| {
            let host = arg_str(args, 0, ctx);
            match resolve_first_ipv4(&host) {
                Some(ip) => Ok(JsValue::from(js_string!(ip.to_string()))),
                None => Ok(JsValue::null()),
            }
        }),
    )?;

    ctx.register_global_callable(
        js_string!("isResolvable"),
        1,
        NativeFunction::from_fn_ptr(|_, args, ctx| {
            let host = arg_str(args, 0, ctx);
            Ok(JsValue::from(resolve_first_ipv4(&host).is_some()))
        }),
    )?;

    ctx.register_global_callable(
        js_string!("myIpAddress"),
        0,
        NativeFunction::from_fn_ptr(|_, _, _| {
            Ok(JsValue::from(js_string!(my_ip_address().to_string())))
        }),
    )?;

    ctx.register_global_callable(
        js_string!("isInNet"),
        3,
        NativeFunction::from_fn_ptr(|_, args, ctx| {
            let host = arg_str(args, 0, ctx);
            let pattern = arg_str(args, 1, ctx);
            let mask = arg_str(args, 2, ctx);
            Ok(JsValue::from(is_in_net(&host, &pattern, &mask)))
        }),
    )?;

    // Conservative time predicates (see fn doc): defined so scripts don't throw.
    for name in ["weekdayRange", "dateRange", "timeRange"] {
        ctx.register_global_callable(
            js_string!(name),
            0,
            NativeFunction::from_fn_ptr(|_, _, _| Ok(JsValue::from(false))),
        )?;
    }

    Ok(())
}

/// Shell-glob match (`*` = any run, `?` = one char) as used by `shExpMatch`.
/// Iterative with backtracking - no regex dependency.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut star_ti): (Option<usize>, usize) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Resolve `host` to its first IPv4 address (blocking; PAC `dnsResolve`/
/// `isResolvable`/`isInNet` are DNS-based). Bounded in practice by the per-host
/// decision cache, so this runs at most once per unique host. May briefly block
/// the calling thread on a slow resolver.
fn resolve_first_ipv4(host: &str) -> Option<Ipv4Addr> {
    // `to_socket_addrs` needs a port; :0 is fine for name resolution only.
    (host, 0u16)
        .to_socket_addrs()
        .ok()?
        .find_map(|addr| match addr.ip() {
            IpAddr::V4(v4) => Some(v4),
            IpAddr::V6(_) => None,
        })
}

/// Best-effort local outbound IPv4 for `myIpAddress`. Opens (but never sends on)
/// a UDP socket to discover which local interface would route outbound; falls
/// back to loopback if that fails.
fn my_ip_address() -> Ipv4Addr {
    let fallback = Ipv4Addr::LOCALHOST;
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0") else {
        return fallback;
    };
    // Connecting a UDP socket only sets the default peer; no packet is sent.
    if socket.connect("8.8.8.8:80").is_err() {
        return fallback;
    }
    match socket.local_addr().map(|a| a.ip()) {
        Ok(IpAddr::V4(v4)) => v4,
        _ => fallback,
    }
}

/// `isInNet(host, pattern, mask)`: resolve `host` to IPv4 and test whether it is
/// in the `pattern`/`mask` network (both dotted-quad IPv4).
fn is_in_net(host: &str, pattern: &str, mask: &str) -> bool {
    let Some(host_ip) = resolve_first_ipv4(host).or_else(|| host.parse().ok()) else {
        return false;
    };
    let Ok(pattern_ip) = pattern.parse::<Ipv4Addr>() else {
        return false;
    };
    let Ok(mask_ip) = mask.parse::<Ipv4Addr>() else {
        return false;
    };
    let host_bits = u32::from(host_ip);
    let pattern_bits = u32::from(pattern_ip);
    let mask_bits = u32::from(mask_ip);
    (host_bits & mask_bits) == (pattern_bits & mask_bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_is_a_noop_and_builds() {
        let cfg = ProxyConfig::system();
        assert!(matches!(cfg, ProxyConfig::System));
        let built = apply_proxy(reqwest::Client::builder(), &cfg)
            .expect("system applies")
            .build();
        assert!(built.is_ok(), "system client builds: {built:?}");
    }

    #[test]
    fn none_applies_no_proxy_and_builds() {
        let cfg = ProxyConfig::None;
        assert!(cfg.is_no_proxy());
        let built = apply_proxy(reqwest::Client::builder(), &cfg)
            .expect("none applies")
            .build();
        assert!(built.is_ok(), "no-proxy client builds: {built:?}");
    }

    #[test]
    fn manual_http_and_socks5_parse_and_build() {
        for url in [
            "http://proxy.corp.example:8080",
            "https://proxy.corp.example:8443",
            "socks5://127.0.0.1:1080",
            "socks5h://proxy.corp.example:1080",
        ] {
            let cfg = ProxyConfig::Manual(url.to_string());
            assert_eq!(cfg.manual_url(), Some(url));
            let built = apply_proxy(reqwest::Client::builder(), &cfg)
                .unwrap_or_else(|e| panic!("apply {url}: {e}"))
                .build();
            assert!(built.is_ok(), "manual {url} builds: {built:?}");
        }
    }

    #[test]
    fn manual_bad_url_fails_closed() {
        // Unsupported scheme.
        let ftp = ProxyConfig::Manual("ftp://proxy:21".to_string());
        assert!(matches!(
            apply_proxy(reqwest::Client::builder(), &ftp),
            Err(ProxyError::UnsupportedScheme { .. })
        ));
        // Not a URL at all.
        let garbage = ProxyConfig::Manual("not a url".to_string());
        assert!(apply_proxy(reqwest::Client::builder(), &garbage).is_err());
        // No host.
        assert!(validate_manual_url("http://").is_err());
    }

    #[test]
    fn validate_manual_url_accepts_supported_schemes() {
        assert!(validate_manual_url("http://h:1").is_ok());
        assert!(validate_manual_url("socks5h://h:1").is_ok());
        assert!(matches!(
            validate_manual_url("socks4://h:1"),
            Err(ProxyError::UnsupportedScheme { .. })
        ));
    }

    #[test]
    fn parse_pac_result_maps_directives() {
        assert_eq!(parse_pac_result("DIRECT"), None);
        assert_eq!(
            parse_pac_result("PROXY p.example:8080"),
            Some("http://p.example:8080".to_string())
        );
        assert_eq!(
            parse_pac_result("HTTPS p.example:8443"),
            Some("https://p.example:8443".to_string())
        );
        assert_eq!(
            parse_pac_result("SOCKS5 s.example:1080"),
            Some("socks5://s.example:1080".to_string())
        );
        // First directive wins; DIRECT-first => direct.
        assert_eq!(parse_pac_result("DIRECT; PROXY p:1"), None);
        assert_eq!(
            parse_pac_result("PROXY a:1; PROXY b:2; DIRECT"),
            Some("http://a:1".to_string())
        );
        // Empty / whitespace => direct.
        assert_eq!(parse_pac_result("   "), None);
    }

    #[test]
    fn glob_match_semantics() {
        assert!(glob_match("*.example.com", "www.example.com"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(glob_match("192.168.*", "192.168.1.5"));
        assert!(!glob_match("*.example.com", "example.com"));
        assert!(glob_match("http://*.internal/*", "http://x.internal/path"));
    }

    #[test]
    fn is_in_net_masks_ipv4() {
        assert!(is_in_net("10.1.2.3", "10.0.0.0", "255.0.0.0"));
        assert!(!is_in_net("11.1.2.3", "10.0.0.0", "255.0.0.0"));
        assert!(is_in_net("192.168.1.50", "192.168.1.0", "255.255.255.0"));
        assert!(!is_in_net("192.168.2.50", "192.168.1.0", "255.255.255.0"));
        // Bad mask / pattern => false, never panic.
        assert!(!is_in_net("10.0.0.1", "not-an-ip", "255.0.0.0"));
    }

    #[test]
    fn compile_rejects_empty_and_missing_function() {
        assert!(matches!(
            PacEngine::compile("   ".to_string(), "t".to_string()),
            Err(ProxyError::PacEmpty { .. })
        ));
        assert!(matches!(
            PacEngine::compile("var x = 1;".to_string(), "t".to_string()),
            Err(ProxyError::PacCompile { .. })
        ));
        // Syntactically broken script.
        assert!(matches!(
            PacEngine::compile("function (".to_string(), "t".to_string()),
            Err(ProxyError::PacCompile { .. })
        ));
    }

    #[test]
    fn pac_engine_routes_internal_vs_external() {
        // A realistic split: internal hosts direct, everything else via proxy.
        let script = r#"
            function FindProxyForURL(url, host) {
                if (isPlainHostName(host) || dnsDomainIs(host, ".corp.example")) {
                    return "DIRECT";
                }
                if (shExpMatch(host, "*.trusted.example")) {
                    return "DIRECT";
                }
                return "PROXY gateway.corp.example:8080";
            }
        "#;
        let engine =
            PacEngine::compile(script.to_string(), "test://inline".to_string()).expect("compiles");

        assert_eq!(engine.evaluate_str("http://intranet/"), None);
        assert_eq!(engine.evaluate_str("http://db.corp.example/x"), None);
        assert_eq!(engine.evaluate_str("https://api.trusted.example/v1"), None);
        assert_eq!(
            engine.evaluate_str("https://www.google.com/"),
            Some("http://gateway.corp.example:8080".to_string())
        );
        // Second lookup for the same host hits the cache (same answer).
        assert_eq!(
            engine.evaluate_str("https://www.google.com/other"),
            Some("http://gateway.corp.example:8080".to_string())
        );
    }

    #[test]
    fn pac_engine_is_usable_in_a_custom_proxy_closure() {
        // The load-bearing Send+Sync regression guard: if PacEngine ever stops
        // being Send+Sync this line fails to compile at exactly this point,
        // not as a cascade through the ~9 downstream build sites.
        let engine = Arc::new(
            PacEngine::compile(
                "function FindProxyForURL(u,h){return \"PROXY p:8080\";}".to_string(),
                "t".to_string(),
            )
            .expect("compiles"),
        );
        let cfg = ProxyConfig::Pac(engine);
        let built = apply_proxy(reqwest::Client::builder(), &cfg)
            .expect("pac applies")
            .build();
        assert!(built.is_ok(), "pac client builds: {built:?}");
    }

    #[test]
    fn manual_url_is_redacted_in_debug() {
        let cfg = ProxyConfig::Manual("http://user:secret@proxy:8080".to_string());
        assert!(!format!("{cfg:?}").contains("secret"));
    }

    #[tokio::test]
    async fn resolve_proxy_modes() {
        let ca = crate::CustomCaConfig::none();
        assert!(matches!(
            resolve_proxy("system", None, &ca).await,
            Ok(ProxyConfig::System)
        ));
        assert!(matches!(
            resolve_proxy("none", None, &ca).await,
            Ok(ProxyConfig::None)
        ));
        assert!(matches!(
            resolve_proxy("manual", Some("http://p:8080"), &ca).await,
            Ok(ProxyConfig::Manual(_))
        ));
        // Manual with no URL => fail closed.
        assert!(matches!(
            resolve_proxy("manual", None, &ca).await,
            Err(ProxyError::MissingManualUrl)
        ));
        // Manual with a bad URL => fail closed.
        assert!(resolve_proxy("manual", Some("ftp://p"), &ca).await.is_err());
        // PAC with no source => fail closed.
        assert!(matches!(
            resolve_proxy("pac", None, &ca).await,
            Err(ProxyError::MissingPacSource)
        ));
        // Unknown mode => System (corrupt-settings default).
        assert!(matches!(
            resolve_proxy("bogus", None, &ca).await,
            Ok(ProxyConfig::System)
        ));
    }

    #[tokio::test]
    async fn resolve_pac_from_local_file() {
        let script = "function FindProxyForURL(url, host){ return \"PROXY local:3128\"; }";
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.pac");
        std::fs::write(&path, script).expect("write pac");

        let ca = crate::CustomCaConfig::none();
        let source = path.to_string_lossy().to_string();
        let cfg = resolve_proxy("pac", Some(&source), &ca)
            .await
            .expect("pac resolves");
        let engine = cfg.pac_engine().expect("is pac");
        assert_eq!(
            engine.evaluate_str("https://anything.example/"),
            Some("http://local:3128".to_string())
        );
        // validate_pac_source also accepts it.
        assert!(validate_pac_source(&source, &ca).await.is_ok());
        // `dir` cleans up on drop.
    }

    #[tokio::test]
    async fn validate_pac_source_rejects_missing_file() {
        let ca = crate::CustomCaConfig::none();
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist.pac");
        assert!(matches!(
            validate_pac_source(&missing.to_string_lossy(), &ca).await,
            Err(ProxyError::PacRead { .. })
        ));
        assert!(validate_pac_source("   ", &ca).await.is_err());
    }

    #[tokio::test]
    async fn resolve_pac_over_http_fetches_compiles_and_evaluates() {
        // Covers the HTTP-fetch branch of `fetch_pac_script` + the process cache
        // in `load_pac_engine` with a one-shot local server (no external network).
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let body = "function FindProxyForURL(u, h){ return \"PROXY served:8080\"; }";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/x-ns-proxy-autoconfig\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        let ca = crate::CustomCaConfig::none();
        let source = format!("http://{addr}/proxy.pac");
        let cfg = resolve_proxy("pac", Some(&source), &ca)
            .await
            .expect("pac resolves over http");
        let engine = cfg.pac_engine().expect("is pac");
        assert_eq!(engine.source(), source);
        assert_eq!(
            engine.evaluate_str("https://x.example/"),
            Some("http://served:8080".to_string())
        );
        let _ = server.join();

        // Second resolve of the SAME source hits the process cache (the one-shot
        // server has already exited, so a re-fetch would fail) - covers the
        // cache-hit branch of `load_pac_engine`.
        let cfg2 = resolve_proxy("pac", Some(&source), &ca)
            .await
            .expect("pac resolves from cache");
        assert!(cfg2.pac_engine().is_some());
    }

    #[test]
    fn pac_helpers_all_execute() {
        // A script that CALLS every registered helper so their native closures
        // are exercised. Uses only numeric/loopback hosts so no external DNS is
        // needed (127.0.0.1 resolves numerically; myIpAddress is local).
        let script = r#"
            function FindProxyForURL(url, host) {
                var checks = [
                    isPlainHostName("intranet"),
                    dnsDomainIs(host, ".example"),
                    localHostOrDomainIs(host, "www.example"),
                    dnsDomainLevels(host) >= 0,
                    shExpMatch(url, "http*"),
                    isResolvable("127.0.0.1"),
                    isInNet("127.0.0.1", "127.0.0.0", "255.0.0.0"),
                    weekdayRange("MON", "FRI"),
                    dateRange(1, 31),
                    timeRange(0, 23)
                ];
                var ip = dnsResolve("127.0.0.1");
                var mine = myIpAddress();
                if (ip !== null && mine.length > 0 && checks[6]) {
                    return "PROXY helpers:9090";
                }
                return "DIRECT";
            }
        "#;
        let engine =
            PacEngine::compile(script.to_string(), "test://helpers".to_string()).expect("compiles");
        assert_eq!(
            engine.evaluate_str("http://www.example/path"),
            Some("http://helpers:9090".to_string())
        );
    }

    #[test]
    fn pac_runtime_throw_falls_back_to_direct() {
        // The script defines FindProxyForURL (so it compiles) but throws when
        // CALLED - `evaluate_str` logs and returns None (direct), covering the
        // run() error path.
        let script = "function FindProxyForURL(u, h){ return h.does.not.exist; }";
        let engine =
            PacEngine::compile(script.to_string(), "test://throw".to_string()).expect("compiles");
        assert_eq!(engine.evaluate_str("https://whatever.example/"), None);
    }

    #[test]
    fn pac_socks_directive_maps_to_socks5() {
        let engine = PacEngine::compile(
            "function FindProxyForURL(u, h){ return \"SOCKS proxy.example:1080\"; }".to_string(),
            "t".to_string(),
        )
        .expect("compiles");
        assert_eq!(
            engine.evaluate_str("https://x.example/"),
            Some("socks5://proxy.example:1080".to_string())
        );
        // A URL with no host is not cacheable/evaluable -> None.
        assert_eq!(engine.evaluate_str("not a url"), None);
    }

    #[test]
    fn file_url_and_http_url_helpers() {
        // as_http_url recognises http/https and rejects a bare path / file URL.
        assert!(as_http_url("http://h/p").is_some());
        assert!(as_http_url("https://h/p").is_some());
        assert!(as_http_url("/etc/proxy.pac").is_none());
        assert!(as_http_url("file:///etc/proxy.pac").is_none());
        // file_url_to_path maps a file URL to a path and passes a bare path through.
        assert_eq!(
            file_url_to_path("/etc/proxy.pac"),
            std::path::PathBuf::from("/etc/proxy.pac")
        );
        let from_file_url = file_url_to_path("file:///etc/proxy.pac");
        assert!(from_file_url.to_string_lossy().contains("proxy.pac"));
    }

    #[test]
    fn ip_helpers_do_not_panic_on_loopback() {
        // Numeric loopback resolves without external DNS; myIpAddress is local.
        assert_eq!(resolve_first_ipv4("127.0.0.1"), Some(Ipv4Addr::LOCALHOST));
        let _ = my_ip_address(); // returns an Ipv4Addr (or loopback fallback)
        assert!(!is_in_net(
            "not-an-ip-and-unresolvable.invalid",
            "10.0.0.0",
            "255.0.0.0"
        ));
    }

    #[test]
    fn proxy_config_debug_and_accessors() {
        assert_eq!(format!("{:?}", ProxyConfig::System), "ProxyConfig::System");
        assert_eq!(format!("{:?}", ProxyConfig::None), "ProxyConfig::None");
        assert!(ProxyConfig::default().manual_url().is_none());
        assert!(ProxyConfig::System.pac_engine().is_none());
        assert!(!ProxyConfig::Manual("http://p:1".into()).is_no_proxy());
        let engine = Arc::new(
            PacEngine::compile(
                "function FindProxyForURL(u,h){return \"DIRECT\";}".to_string(),
                "t".to_string(),
            )
            .expect("compiles"),
        );
        let pac = ProxyConfig::Pac(engine);
        assert!(pac.pac_engine().is_some());
        assert!(pac.manual_url().is_none());
        assert!(format!("{pac:?}").contains("Pac"));
    }

    #[test]
    fn parse_pac_result_skips_unknown_then_takes_next() {
        // An unknown directive is skipped; the following known one is used.
        assert_eq!(
            parse_pac_result("FOOBAR x:1; PROXY y:2"),
            Some("http://y:2".to_string())
        );
        // HTTP directive alias maps to http.
        assert_eq!(parse_pac_result("HTTP p:3"), Some("http://p:3".to_string()));
    }
}
