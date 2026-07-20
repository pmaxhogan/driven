// Driven anonymous-telemetry ingest Worker (SPEC s16, ROADMAP M9b).
//
// Receives the opt-out-able usage ping the Driven desktop client POSTs on startup
// and every 24h, validates its shape, and writes the data points to a Cloudflare
// Analytics Engine dataset. It is the server side of `src-tauri/src/telemetry.rs`.
//
// PRIVACY (load-bearing, SPEC s16): the payload carries ONLY counts, sizes, error
// CODES, and latencies - never a file name, path, or content. This Worker is
// careful not to log raw bodies (there is no PII by design, but be safe) and
// caps the request body size so a malformed / hostile client cannot flood it.
//
// Routing (SPEC s16): deployed to `driven.maxhogan.dev/telemetry/*` on the Driven
// Cloudflare account (account id 9c20c14daa20466a2d761a47162f719a). The Worker
// ROUTE takes precedence over the CF Pages site on the same hostname for the
// `/telemetry/*` path prefix, so the Pages site keeps serving the root + /updates
// while this Worker owns /telemetry/* (see wrangler.jsonc).
//
// DEPLOYMENT IS DEFERRED to M10/ops (it needs `wrangler deploy` + CF creds). This
// module is IMPLEMENTED + statically validated (tsc + a unit test of the handler
// against a mocked AE binding) now; the live deploy + e2e telemetry validation
// happen at M10. See design/CODEX_NOTES.md "## M9b - telemetry".

/// The Worker environment bindings (wrangler.jsonc + secrets). `TELEMETRY` is the
/// Analytics Engine dataset the validated ping is WRITTEN to (the binding).
///
/// The `/stats/latency` rollup READ path (DESIGN s13) cannot read the AE dataset
/// through the write binding - AE reads go through the SQL HTTP API - so it needs:
/// - `QUERY_TOKEN`: the bearer secret gating the endpoint (it exposes aggregate
///   telemetry, so it is NEVER served unauthenticated). Unset => the endpoint 503s.
/// - `CF_API_TOKEN`: a Cloudflare API token with Account Analytics read, used to
///   call the AE SQL API. Unset => the endpoint 503s.
/// - `CF_ACCOUNT_ID`: the account id for the SQL API URL; defaults to the Driven
///   account when unset.
/// All three are wrangler secrets/vars set post-deploy (see the worker README);
/// they are optional in the type so the ingest path keeps working without them.
export interface Env {
  TELEMETRY: AnalyticsEngineDataset;
  QUERY_TOKEN?: string;
  CF_API_TOKEN?: string;
  CF_ACCOUNT_ID?: string;
}

/// The ingest path (SPEC s16): POST a ping here.
const PING_PATH = "/telemetry/v1/ping";

/// The latency-rollup read path (DESIGN s13): GET per-day aggregates here. Scoped
/// under `/telemetry/*` because that is the only prefix the Worker route serves
/// (wrangler.jsonc); the documented `GET /stats/latency` maps here.
const STATS_LATENCY_PATH = "/telemetry/v1/stats/latency";

/// The Analytics Engine dataset name (matches wrangler.jsonc `dataset`). Used in
/// the SQL API `FROM` clause for the rollup query.
const DATASET = "driven_telemetry";

/// The Driven Cloudflare account id (wrangler.jsonc `account_id`), the default
/// target for the AE SQL API when `CF_ACCOUNT_ID` is not set.
const DEFAULT_ACCOUNT_ID = "9c20c14daa20466a2d761a47162f719a";

/// `/stats/latency` `days` window: default and hard cap (SPEC/DESIGN s13). The
/// query looks back `days` days; the value is clamped to `[1, 90]`.
const DEFAULT_STATS_DAYS = 7;
const MAX_STATS_DAYS = 90;

/// Max accepted request body size (bytes). The real ping is well under 4 KB; this
/// cap rejects a hostile / malformed oversized body before parsing (SPEC s16
/// "cap body size").
const MAX_BODY_BYTES = 16 * 1024;

/// The validated, privacy-safe shape of the ping payload (SPEC s16). Only the
/// fields written to Analytics Engine are typed here; unknown extra fields are
/// ignored (forward compatibility), never logged.
interface PingPayload {
  install_id: string;
  ts: number;
  version: string;
  os: string;
  os_version: string | null;
  arch: string;
  channel: string;
  events_24h: {
    files_uploaded: number;
    bytes_uploaded: number;
    errors_by_class: Record<string, number>;
    deep_verify_runs: number;
    // SPEC s16: update_applied is a BOOLEAN (an in-app update was applied in the
    // window or not). Kept byte-consistent with the Rust client payload.
    update_applied: boolean;
  };
  latency_p50_p95_ms: {
    scan: number[];
    upload_per_mb: number[];
  };
}

// --------------------------------------------------------------------------
// PUBLIC-ENDPOINT HARDENING (M9b P1-1): this Worker is on a public hostname, so
// validatePing must reject ANYTHING that could persist PII / high-cardinality
// junk into Analytics Engine. Every accepted field is either a UUID v4, a value
// from a closed whitelist, a SPEC s24 error CODE, or a bounded number. Anything
// else is a 400. (The payload is privacy-safe BY CONTRACT from the client, but a
// public endpoint must not TRUST the client.)
// --------------------------------------------------------------------------

/// UUID v4 (RFC 4122 variant) - the only accepted `install_id` shape (SPEC s16:
/// "a UUID v4 minted on first run"). Lowercase hex; version nibble 4; variant
/// nibble 8/9/a/b. Rejects path/email-shaped or arbitrary strings.
const UUID_V4 = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

/// Closed whitelist for `channel` (SPEC s15 updater channels).
const CHANNELS = new Set(["stable", "dev"]);

/// Closed whitelist for `os` (SPEC s16 `os` family; matches Rust
/// `std::env::consts::OS` for the platforms Driven ships on).
const OS_FAMILIES = new Set(["windows", "macos", "linux"]);

/// Closed whitelist for `arch` (SPEC s16 `arch`; matches Rust
/// `std::env::consts::ARCH` for the targets Driven ships on).
const ARCHES = new Set(["x86_64", "aarch64"]);

/// Max accepted `version` string length (semver + a channel suffix is short).
const MAX_VERSION_LEN = 64;

/// Max accepted `os_version` string length (e.g. "11.26200", "14.5", a kernel
/// string). Bounded so a hostile client cannot stuff a path/PII here.
const MAX_OS_VERSION_LEN = 64;

/// Strict content allowlist for `version` (M9b P1-2). Length is bounded above; the
/// SHAPE must match what `src-tauri/src/telemetry.rs` actually emits, which is the
/// crate version from `AppHandle::package_info().version` - a semver `MAJOR.MINOR.PATCH`
/// optionally followed by a dot-separated alphanumeric/hyphen prerelease and/or a
/// `+build` suffix. The CI dev channel emits e.g. `0.1.1-dev.123.ab0c9f1`. This is a
/// pragmatic semver-ish allowlist: a leading `MAJOR.MINOR.PATCH` of digits, then an
/// OPTIONAL `-prerelease` of dot-separated `[0-9A-Za-z-]` identifiers, then an
/// OPTIONAL `+build` of dot-separated `[0-9A-Za-z-]` identifiers. It REJECTS `/`,
/// `\`, `@`, whitespace, and control chars (none of those appear in a crate version),
/// so a short PII string like `alice@example.com` or `/home/alice` cannot pass.
const VERSION_RE = /^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/;

/// Strict content allowlist for `os_version` (M9b P1-2). The client collects this
/// via the `os_info` crate `Version` (`src-tauri/src/telemetry.rs::coarse_os_version`),
/// which renders as either a dotted numeric build (e.g. Windows `11.26200`, macOS
/// `14.5`, Linux `10.0.19045`) OR a short `Custom` string for some distros (e.g.
/// `rolling`, a codename, or `22.04 LTS`). So the allowed charset is COARSE platform
/// chars only: ASCII letters, digits, dots, hyphens, underscores, and single spaces
/// between tokens. It REJECTS `/`, `\`, `@`, control chars, and whitespace-RUNS
/// (so a path/email/PII string with separators or padding is a 400). The leading and
/// trailing char must be alphanumeric so a value cannot start/end with a separator.
const OS_VERSION_RE = /^[0-9A-Za-z](?:[0-9A-Za-z._-]| (?! ))*[0-9A-Za-z]$|^[0-9A-Za-z]$/;

/// Max number of distinct `errors_by_class` keys accepted (the s24 code set is
/// ~44; this cap rejects a high-cardinality flood while leaving headroom).
const MAX_ERROR_CLASSES = 64;

/// Max accepted per-class error count (a sane 24h-window upper bound; rejects an
/// absurd value that could skew the dataset).
const MAX_ERROR_COUNT = 1_000_000_000;

/// Per-field numeric caps for the `events_24h` aggregates (M9b P2-2). Each is a
/// sane 24h-window upper bound; a value above it (or a fraction / non-safe-integer)
/// is a 400 so a hostile client cannot poison the bounded-integer AE measures with
/// fractions or huge finite doubles. `bytes_uploaded` allows up to ~1 PiB/day; the
/// counts are generous but finite.
const MAX_FILES_UPLOADED = 1_000_000_000;
const MAX_BYTES_UPLOADED = 1_125_899_906_842_624; // 1 PiB (under Number.MAX_SAFE_INTEGER)
const MAX_DEEP_VERIFY_RUNS = 1_000_000;

/// Sane `ts` (Unix epoch MILLISECONDS) window (M9b P2-2). The client sends
/// `SystemClock.now_ms()`, so reject anything outside a plausible range: from
/// 2020-01-01 to 2100-01-01. This catches a seconds-vs-ms mistake, a fraction, a
/// huge finite double, or an absurd far-future/past timestamp.
const TS_MIN_MS = 1_577_836_800_000; // 2020-01-01T00:00:00Z
const TS_MAX_MS = 4_102_444_800_000; // 2100-01-01T00:00:00Z

/// The closed set of SPEC s24 error codes (the ONLY accepted `errors_by_class`
/// keys). MUST mirror `crates/driven-core/src/types.rs` `ErrorCode::code()` - the
/// codes the Rust client actually emits as `activity_log.event_type` for an
/// error-level row. Renaming/removing a code is a breaking i18n change (SPEC
/// s24), so this list is append-only in lockstep with the Rust enum.
const ERROR_CODES = new Set([
  "auth.invalid_grant",
  "auth.consent_required",
  "auth.network_unreachable",
  "drive.rate_limited",
  "drive.daily_quota_exhausted",
  "drive.quota_exhausted",
  "drive.upload_size_limit",
  "drive.checksum_mismatch",
  "drive.unreachable",
  "drive.resumable_session_invalid",
  "drive.dest_folder_missing",
  "drive.dest_folder_permission_denied",
  "local.file_locked",
  "local.vss_unavailable",
  "local.file_changed_during_upload",
  "local.file_replaced_during_upload",
  "local.io_error",
  "local.path_too_long",
  "local.unicode_collision",
  "local.disk_full",
  "local.invalid_filename",
  "local.ads_skipped",
  "net.offline",
  "net.no_internet",
  "net.dns_failed",
  "net.captive_portal",
  "net.timeout",
  "net.intermittent",
  "net.proxy_required",
  "update.endpoint_unreachable",
  "update.signature_invalid",
  "update.manual_required_macos",
  "crypto.key_missing",
  "crypto.decrypt_failed",
  "crypto.recovery_phrase_invalid",
  "state.db_locked",
  "state.db_corrupt",
  "state.reconcile_orphan",
  "harness.timeout",
  "internal.bug",
  "internal.invalid_input",
]);

/// A small JSON Response helper that never echoes the request body.
function json(status: number, body: Record<string, unknown>): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

/// Type guard: is `v` a bounded, non-negative SAFE integer in `[0, max]`? (M9b
/// P2-2: counts/sizes must be a non-negative integer representable exactly as a JS
/// number - `Number.isSafeInteger` rejects fractions AND huge finite doubles that
/// would round - and under a per-field cap, so the bounded-integer AE measures
/// stay clean.)
function isBoundedCount(v: unknown, max: number): v is number {
  return typeof v === "number" && Number.isSafeInteger(v) && v >= 0 && v <= max;
}

/// Type guard: is `v` a SAFE integer within `[min, max]`? (M9b P2-2: used for the
/// `ts` epoch-ms range so a fraction, a huge double, or an absurd timestamp is a
/// 400.)
function isIntegerInRange(v: unknown, min: number, max: number): v is number {
  return typeof v === "number" && Number.isSafeInteger(v) && v >= min && v <= max;
}

/// Validate the parsed JSON into a [`PingPayload`], or return a reason string for
/// a 400.
///
/// PUBLIC-ENDPOINT HARDENING (M9b P1-1): because the endpoint is public, this is
/// strict on EVERY field - `install_id` must be a UUID v4; `channel` / `os` /
/// `arch` must be from a closed whitelist; `version` / `os_version` are length-
/// bounded; `errors_by_class` keys must be in the SPEC s24 code set with a capped
/// key count and a bounded per-class value; numeric aggregates must be bounded
/// non-negative integers. Anything else is a 400 so no PII / high-cardinality
/// junk reaches Analytics Engine. Tolerant only of an empty latency array (V1
/// sends empty arrays) and a null/absent `os_version`. Never throws.
export function validatePing(value: unknown): { ok: true; payload: PingPayload } | { ok: false; reason: string } {
  if (typeof value !== "object" || value === null) {
    return { ok: false, reason: "body is not a JSON object" };
  }
  const v = value as Record<string, unknown>;

  // install_id MUST be a UUID v4 (rejects path/email/arbitrary-string ids).
  if (typeof v.install_id !== "string" || !UUID_V4.test(v.install_id)) {
    return { ok: false, reason: "install_id" };
  }
  // ts: a SAFE integer within a plausible epoch-ms window (M9b P2-2). Rejects a
  // fraction, a huge finite double, a seconds-vs-ms mistake, and an absurd date.
  if (!isIntegerInRange(v.ts, TS_MIN_MS, TS_MAX_MS)) return { ok: false, reason: "ts" };
  // version: bounded length AND a strict semver-ish content allowlist (M9b P1-2).
  // Length-bounded first, then the regex (which also forbids /, \, @, whitespace,
  // control chars) so a short PII string cannot pass the length check.
  if (
    typeof v.version !== "string" ||
    v.version.length === 0 ||
    v.version.length > MAX_VERSION_LEN ||
    !VERSION_RE.test(v.version)
  ) {
    return { ok: false, reason: "version" };
  }
  // os / arch / channel: closed whitelists (rejects arbitrary platform strings).
  if (typeof v.os !== "string" || !OS_FAMILIES.has(v.os)) return { ok: false, reason: "os" };
  if (typeof v.arch !== "string" || !ARCHES.has(v.arch)) return { ok: false, reason: "arch" };
  if (typeof v.channel !== "string" || !CHANNELS.has(v.channel)) return { ok: false, reason: "channel" };

  // os_version: optional; when present it must be a bounded string (or null) AND
  // match the coarse platform-version content allowlist (M9b P1-2). The regex
  // forbids /, \, @, control chars, and whitespace-runs, so a path/email/PII
  // string cannot pass the length check.
  let osVersion: string | null = null;
  if (v.os_version !== undefined && v.os_version !== null) {
    if (
      typeof v.os_version !== "string" ||
      v.os_version.length === 0 ||
      v.os_version.length > MAX_OS_VERSION_LEN ||
      !OS_VERSION_RE.test(v.os_version)
    ) {
      return { ok: false, reason: "os_version" };
    }
    osVersion = v.os_version;
  }

  const e = v.events_24h;
  if (typeof e !== "object" || e === null) return { ok: false, reason: "events_24h" };
  const ev = e as Record<string, unknown>;
  // M9b P2-2: each aggregate must be a bounded, non-negative SAFE integer under
  // its per-field cap (rejects fractions, huge finite doubles, and absurd values).
  if (!isBoundedCount(ev.files_uploaded, MAX_FILES_UPLOADED)) {
    return { ok: false, reason: "events_24h.files_uploaded" };
  }
  if (!isBoundedCount(ev.bytes_uploaded, MAX_BYTES_UPLOADED)) {
    return { ok: false, reason: "events_24h.bytes_uploaded" };
  }
  if (!isBoundedCount(ev.deep_verify_runs, MAX_DEEP_VERIFY_RUNS)) {
    return { ok: false, reason: "events_24h.deep_verify_runs" };
  }
  // SPEC s16: update_applied is a BOOLEAN (byte-consistent with the Rust client).
  if (typeof ev.update_applied !== "boolean") return { ok: false, reason: "events_24h.update_applied" };
  if (typeof ev.errors_by_class !== "object" || ev.errors_by_class === null || Array.isArray(ev.errors_by_class)) {
    return { ok: false, reason: "events_24h.errors_by_class" };
  }
  // errors_by_class: keys MUST be SPEC s24 error codes; values bounded counts;
  // key count capped (no high-cardinality flood).
  const errEntries = Object.entries(ev.errors_by_class as Record<string, unknown>);
  if (errEntries.length > MAX_ERROR_CLASSES) {
    return { ok: false, reason: "events_24h.errors_by_class.too_many" };
  }
  const errors: Record<string, number> = {};
  for (const [k, n] of errEntries) {
    if (!ERROR_CODES.has(k)) return { ok: false, reason: `events_24h.errors_by_class[${k}]` };
    if (!isBoundedCount(n, MAX_ERROR_COUNT)) return { ok: false, reason: `events_24h.errors_by_class[${k}]` };
    errors[k] = n;
  }

  const l = v.latency_p50_p95_ms;
  if (typeof l !== "object" || l === null) return { ok: false, reason: "latency_p50_p95_ms" };
  const lv = l as Record<string, unknown>;
  if (!Array.isArray(lv.scan)) return { ok: false, reason: "latency_p50_p95_ms.scan" };
  if (!Array.isArray(lv.upload_per_mb)) return { ok: false, reason: "latency_p50_p95_ms.upload_per_mb" };

  return {
    ok: true,
    payload: {
      install_id: v.install_id,
      ts: v.ts as number,
      version: v.version,
      os: v.os,
      os_version: osVersion,
      arch: v.arch,
      channel: v.channel,
      events_24h: {
        files_uploaded: ev.files_uploaded as number,
        bytes_uploaded: ev.bytes_uploaded as number,
        errors_by_class: errors,
        deep_verify_runs: ev.deep_verify_runs as number,
        update_applied: ev.update_applied,
      },
      latency_p50_p95_ms: {
        scan: (lv.scan as unknown[]).filter((n): n is number => typeof n === "number"),
        upload_per_mb: (lv.upload_per_mb as unknown[]).filter(
          (n): n is number => typeof n === "number",
        ),
      },
    },
  };
}

/// Total error count across all classes (a single Analytics Engine "double").
function totalErrors(errors: Record<string, number>): number {
  let sum = 0;
  for (const n of Object.values(errors)) sum += n;
  return sum;
}

/// The value written to a latency percentile double when the client reported NO
/// samples for that metric this window (its array is empty). A NEGATIVE sentinel
/// so the rollup query can distinguish "no samples" (`< 0`) from a LEGIT `0 ms`
/// (a sub-millisecond per-file scan rounds to 0) - the query filters `>= 0`.
const NO_LATENCY = -1;

/// Extract `[p50, p95]` from a client latency array, mapping an empty (or
/// malformed short) array to the [`NO_LATENCY`] sentinel pair (DESIGN s13).
function latencyPair(arr: number[]): [number, number] {
  if (arr.length >= 2) return [arr[0], arr[1]];
  return [NO_LATENCY, NO_LATENCY];
}

/// Write one validated ping to Analytics Engine (SPEC s16, DESIGN s13). Schema:
/// - indexes: [install_id]   (the sampling/grouping key - anonymous)
/// - blobs:   [os, arch, channel, version, os_version, errors_by_class JSON]
///            (low-card dims; os_version is "" when the client did not send one)
/// - doubles: [files_uploaded, bytes_uploaded, deep_verify_runs, update_applied,
///             total_errors, ts,                              // double1..double6
///             scan_p50, scan_p95, upload_per_mb_p50, upload_per_mb_p95]
///                                                            // double7..double10
///   The 4 latency doubles are appended (never reordered) so existing columns keep
///   their positions; an absent metric writes the NO_LATENCY (-1) sentinel.
/// Writes are non-blocking (no await / waitUntil needed per the CF docs).
export function writePing(env: Env, p: PingPayload): void {
  const [scanP50, scanP95] = latencyPair(p.latency_p50_p95_ms.scan);
  const [upP50, upP95] = latencyPair(p.latency_p50_p95_ms.upload_per_mb);
  env.TELEMETRY.writeDataPoint({
    indexes: [p.install_id],
    blobs: [
      p.os,
      p.arch,
      p.channel,
      p.version,
      // Coarse OS version (e.g. "11.26200"); bounded + non-PII, "" when absent.
      p.os_version ?? "",
      // The error-code -> count map as JSON (codes are a fixed enum; never PII).
      JSON.stringify(p.events_24h.errors_by_class),
    ],
    doubles: [
      p.events_24h.files_uploaded,
      p.events_24h.bytes_uploaded,
      p.events_24h.deep_verify_runs,
      // SPEC s16: update_applied is a boolean; AE doubles are numeric, so it is
      // stored as 0/1 (the dataset column stays a clean 0-or-1 flag).
      p.events_24h.update_applied ? 1 : 0,
      totalErrors(p.events_24h.errors_by_class),
      p.ts,
      // DESIGN s13: the client-reported [p50, p95] latency percentiles (ms), or
      // the NO_LATENCY sentinel when the client had no samples this window.
      scanP50,
      scanP95,
      upP50,
      upP95,
    ],
  });
}

/// Read the request body from the stream with a BYTE counter, rejecting as soon
/// as the cumulative byte count exceeds `maxBytes` (M9b P2-2). This is byte-
/// accurate (it counts the raw bytes of each chunk, NOT UTF-16 code units), so a
/// multibyte / chunked body that lies about (or omits) its Content-Length cannot
/// slip past the cap. On success returns the UTF-8-decoded text; on an oversized
/// body returns `body_too_large`; on a stream error returns `unreadable_body`.
/// Never throws.
async function readBodyCapped(
  request: Request,
  maxBytes: number,
): Promise<{ ok: true; text: string } | { ok: false; error: string }> {
  const body = request.body;
  // No stream (e.g. an empty body): decode whatever text() yields (it is empty
  // or tiny), still guarding against a surprise oversized read.
  if (body === null) {
    try {
      const text = await request.text();
      // Byte-measure via the UTF-8 encoder (text() already buffered it, but the
      // byte length is what the cap is about).
      if (new TextEncoder().encode(text).length > maxBytes) {
        return { ok: false, error: "body_too_large" };
      }
      return { ok: true, text };
    } catch {
      return { ok: false, error: "unreadable_body" };
    }
  }

  const reader = body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      if (value) {
        total += value.byteLength;
        // Stop the moment we exceed the cap (we read at most maxBytes+1 bytes
        // worth of chunks; we do not keep draining a hostile unbounded stream).
        if (total > maxBytes) {
          try {
            await reader.cancel();
          } catch {
            // Best-effort cancel; ignore.
          }
          return { ok: false, error: "body_too_large" };
        }
        chunks.push(value);
      }
    }
  } catch {
    return { ok: false, error: "unreadable_body" };
  }

  // Concatenate the collected chunks and UTF-8 decode.
  const buf = new Uint8Array(total);
  let offset = 0;
  for (const c of chunks) {
    buf.set(c, offset);
    offset += c.byteLength;
  }
  // UTF-8 decode (non-fatal: replaces invalid sequences with U+FFFD rather than
  // throwing, so a body with bad bytes still parses-or-fails downstream as JSON).
  const text = new TextDecoder().decode(buf);
  return { ok: true, text };
}

// --------------------------------------------------------------------------
// LATENCY ROLLUP (DESIGN s13): a gated, read-only per-day aggregate of the
// client-reported scan / upload-per-MB percentiles, over the Analytics Engine
// SQL API. NEVER served unauthenticated (it exposes aggregate telemetry).
// --------------------------------------------------------------------------

/// One metric's per-day aggregate row returned by `GET /stats/latency`.
interface LatencyDayRow {
  /// The UTC day, `YYYY-MM-DD`.
  day: string;
  /// Mean of the pinged p50s that day (ms).
  avg_p50_ms: number;
  /// Mean of the pinged p95s that day (ms).
  avg_p95_ms: number;
  /// Worst pinged p95 that day (ms).
  max_p95_ms: number;
  /// Number of pings that reported this metric that day.
  samples: number;
}

/// Constant-time-ish string equality (avoids leaking the token via early-exit
/// timing on a per-char compare). Length difference is not hidden - fine for a
/// random bearer token.
function safeEqual(a: string, b: string): boolean {
  if (a.length !== b.length) return false;
  let mismatch = 0;
  for (let i = 0; i < a.length; i++) {
    mismatch |= a.charCodeAt(i) ^ b.charCodeAt(i);
  }
  return mismatch === 0;
}

/// Clamp the `days` query param to a validated integer in `[1, MAX_STATS_DAYS]`,
/// defaulting to [`DEFAULT_STATS_DAYS`] when absent / non-numeric. Interpolated
/// into the SQL string, so it MUST be a bounded integer (there are no bind params
/// on the AE SQL API).
function clampStatsDays(raw: string | null): number {
  // Absent / blank -> default (note `Number(null)` and `Number("")` are 0, NOT
  // NaN, so these must be handled before the numeric parse).
  if (raw === null || raw.trim() === "") return DEFAULT_STATS_DAYS;
  const n = Number(raw);
  if (!Number.isFinite(n)) return DEFAULT_STATS_DAYS;
  const i = Math.trunc(n);
  if (i < 1) return 1;
  if (i > MAX_STATS_DAYS) return MAX_STATS_DAYS;
  return i;
}

/// Query one latency metric's per-day aggregates over the AE SQL API. `p50Col` /
/// `p95Col` are the AE double column names for this metric (e.g. `double7` /
/// `double8`). Rows carrying the NO_LATENCY sentinel (`< 0`, an empty-latency
/// ping) are excluded via `WHERE p50 >= 0`, so a legit `0 ms` still counts. The
/// response is the CF `{ meta, data }` JSON (NOT ndjson); each `data[]` row's
/// numeric columns arrive as strings, so they are coerced with `Number`.
async function queryLatencyMetric(
  env: Env,
  accountId: string,
  days: number,
  p50Col: string,
  p95Col: string,
): Promise<LatencyDayRow[]> {
  // `days` is a validated integer (clampStatsDays) and the column names are
  // internal constants, so this interpolation carries no injection surface.
  const sql =
    `SELECT toDate(timestamp) AS day, ` +
    `AVG(${p50Col}) AS avg_p50, ` +
    `AVG(${p95Col}) AS avg_p95, ` +
    `MAX(${p95Col}) AS max_p95, ` +
    `COUNT() AS samples ` +
    `FROM ${DATASET} ` +
    `WHERE timestamp > NOW() - INTERVAL '${days}' DAY AND ${p50Col} >= 0 ` +
    `GROUP BY day ORDER BY day`;

  const resp = await fetch(
    `https://api.cloudflare.com/client/v4/accounts/${accountId}/analytics_engine/sql`,
    {
      method: "POST",
      headers: { Authorization: `Bearer ${env.CF_API_TOKEN}` },
      body: sql,
    },
  );
  if (!resp.ok) {
    throw new Error(`analytics_engine sql query failed: ${resp.status}`);
  }
  const body = (await resp.json()) as { data?: Array<Record<string, unknown>> };
  const rows = Array.isArray(body.data) ? body.data : [];
  return rows.map((r) => ({
    day: String(r.day),
    avg_p50_ms: Number(r.avg_p50),
    avg_p95_ms: Number(r.avg_p95),
    max_p95_ms: Number(r.max_p95),
    samples: Number(r.samples),
  }));
}

/// Handle `GET /stats/latency?days=N` (DESIGN s13). AUTH: a `Bearer QUERY_TOKEN`
/// header. Misconfiguration (no `QUERY_TOKEN` or no `CF_API_TOKEN`) is a 503 -
/// the endpoint is NEVER served open. Returns per-day aggregates for both metrics.
export async function handleStatsLatency(request: Request, env: Env): Promise<Response> {
  // Misconfigured => 503 (never fall through to an unauthenticated / broken read).
  if (!env.QUERY_TOKEN || !env.CF_API_TOKEN) {
    return json(503, { error: "stats_not_configured" });
  }
  // Bearer auth against the QUERY_TOKEN secret.
  const auth = request.headers.get("authorization");
  if (!auth || !safeEqual(auth, `Bearer ${env.QUERY_TOKEN}`)) {
    return new Response(JSON.stringify({ error: "unauthorized" }), {
      status: 401,
      headers: { "content-type": "application/json", "www-authenticate": "Bearer" },
    });
  }

  const url = new URL(request.url);
  const days = clampStatsDays(url.searchParams.get("days"));
  const accountId = env.CF_ACCOUNT_ID ?? DEFAULT_ACCOUNT_ID;

  try {
    // Two queries (one per metric) so each filters its OWN sentinel column - a
    // shared WHERE could not exclude an empty-scan ping without also dropping its
    // (present) upload sample, and vice versa. Runs them concurrently.
    const [scan, uploadPerMb] = await Promise.all([
      queryLatencyMetric(env, accountId, days, "double7", "double8"),
      queryLatencyMetric(env, accountId, days, "double9", "double10"),
    ]);
    return json(200, { days, metrics: { scan, upload_per_mb: uploadPerMb } });
  } catch {
    // Never echo the query / token; a generic upstream-failure signal.
    console.error("telemetry: stats/latency AE query failed");
    return json(502, { error: "query_failed" });
  }
}

/// The Worker request handler (SPEC s16, DESIGN s13). Pure-ish: takes `request` +
/// `env`, so a unit test drives it with a mocked AE binding + mocked `fetch` (no
/// live runtime, no network).
///
/// Contract:
/// - POST /telemetry/v1/ping with a valid JSON body -> write to AE, 204.
/// - POST /telemetry/v1/ping with a malformed body / oversized body -> 400.
/// - GET  /telemetry/v1/stats/latency (Bearer QUERY_TOKEN) -> per-day rollup, 200.
/// - the right path but the wrong method -> 405.
/// - any other path -> 404.
export async function handle(request: Request, env: Env): Promise<Response> {
  const url = new URL(request.url);

  // DESIGN s13: the gated latency-rollup read path. GET only (405 otherwise).
  if (url.pathname === STATS_LATENCY_PATH) {
    if (request.method !== "GET") {
      return new Response(JSON.stringify({ error: "method_not_allowed" }), {
        status: 405,
        headers: { "content-type": "application/json", allow: "GET" },
      });
    }
    return handleStatsLatency(request, env);
  }

  // Only the ping path exists; everything else is 404 (the CF Pages site serves
  // the root + /updates; this Worker owns only /telemetry/*).
  if (url.pathname !== PING_PATH) {
    return json(404, { error: "not_found" });
  }

  // Only POST is accepted on the ping path; anything else is 405.
  if (request.method !== "POST") {
    return new Response(JSON.stringify({ error: "method_not_allowed" }), {
      status: 405,
      headers: { "content-type": "application/json", allow: "POST" },
    });
  }

  // Cap the body size BY BYTES before parsing (SPEC s16; M9b P2-2). Two checks:
  //  1. An EXPLICIT Content-Length that is invalid or over the cap is a 400 up
  //     front (no read at all). A negative / NaN / non-integer length is rejected
  //     as malformed rather than trusted.
  //  2. Because the header can be absent or LIE (chunked transfer / a hostile
  //     client), the actual body is read from the stream with a BYTE counter that
  //     stops as soon as it exceeds the cap (reading at most MAX_BODY_BYTES+1
  //     bytes), then decoded. `raw.length` (UTF-16 code units) is NEVER used as a
  //     byte cap - a multibyte body could exceed the byte cap while staying under
  //     it in code units.
  const clHeader = request.headers.get("content-length");
  if (clHeader !== null) {
    const declaredLen = Number(clHeader);
    if (!Number.isInteger(declaredLen) || declaredLen < 0) {
      return json(400, { error: "invalid_content_length" });
    }
    if (declaredLen > MAX_BODY_BYTES) {
      return json(400, { error: "body_too_large" });
    }
  }

  const bodyResult = await readBodyCapped(request, MAX_BODY_BYTES);
  if (!bodyResult.ok) {
    return json(400, { error: bodyResult.error });
  }
  const raw = bodyResult.text;

  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    // Do NOT echo the raw body (privacy + safety).
    return json(400, { error: "invalid_json" });
  }

  const result = validatePing(parsed);
  if (!result.ok) {
    return json(400, { error: "invalid_payload", field: result.reason });
  }

  // Write to Analytics Engine. M9b P2-3: if the write THROWS (e.g. a
  // misconfigured / missing AE binding), return a 5xx - do NOT 204. A 204 makes
  // the client treat the window as delivered and advance `last_sent_at`, so a
  // broken binding would permanently DROP that window while looking healthy. A
  // 5xx makes the client NOT checkpoint; it does not retry-storm (it simply waits
  // for the next scheduled tick), so the window is retried later rather than lost.
  try {
    writePing(env, result.payload);
  } catch {
    // Intentionally generic - never log the payload/body.
    console.error("telemetry: writeDataPoint failed");
    return json(503, { error: "write_failed" });
  }

  // 204 No Content: accepted + written, nothing to return.
  return new Response(null, { status: 204 });
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    return handle(request, env);
  },
};
