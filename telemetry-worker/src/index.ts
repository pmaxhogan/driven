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

/// The Worker environment bindings (wrangler.jsonc). `TELEMETRY` is the Analytics
/// Engine dataset the validated ping is written to.
export interface Env {
  TELEMETRY: AnalyticsEngineDataset;
}

/// The only path this Worker serves (SPEC s16). Anything else is 404.
const PING_PATH = "/telemetry/v1/ping";

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

/// Write one validated ping to Analytics Engine (SPEC s16). The dataset schema:
/// - indexes: [install_id]   (the sampling/grouping key - anonymous)
/// - blobs:   [os, arch, channel, version, os_version, errors_by_class JSON]
///            (low-card dims; os_version is "" when the client did not send one)
/// - doubles: [files_uploaded, bytes_uploaded, deep_verify_runs, update_applied,
///             total_errors, ts]  (the numeric measures; update_applied is 0/1)
/// Writes are non-blocking (no await / waitUntil needed per the CF docs).
export function writePing(env: Env, p: PingPayload): void {
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
    ],
  });
}

/// The Worker request handler (SPEC s16). Pure-ish: takes `request` + `env`, so a
/// unit test drives it with a mocked AE binding (no live runtime, no network).
///
/// Contract:
/// - POST /telemetry/v1/ping with a valid JSON body -> write to AE, 204.
/// - POST /telemetry/v1/ping with a malformed body / oversized body -> 400.
/// - the right path but the wrong method -> 405.
/// - any other path -> 404.
export async function handle(request: Request, env: Env): Promise<Response> {
  const url = new URL(request.url);

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

  // Cap the body size before reading it fully (SPEC s16). A declared
  // Content-Length over the cap is rejected up front; the actual read is also
  // bounded below in case the header lies.
  const declaredLen = Number(request.headers.get("content-length") ?? "0");
  if (Number.isFinite(declaredLen) && declaredLen > MAX_BODY_BYTES) {
    return json(400, { error: "body_too_large" });
  }

  let raw: string;
  try {
    raw = await request.text();
  } catch {
    return json(400, { error: "unreadable_body" });
  }
  if (raw.length > MAX_BODY_BYTES) {
    return json(400, { error: "body_too_large" });
  }

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

  // Write to Analytics Engine. A binding error must not 500 the client (the ping
  // is best-effort on the client side too); log a generic message (no body) and
  // still return success so the client does not retry-storm.
  try {
    writePing(env, result.payload);
  } catch {
    // Intentionally generic - never log the payload/body.
    console.error("telemetry: writeDataPoint failed");
  }

  // 204 No Content: accepted, nothing to return.
  return new Response(null, { status: 204 });
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    return handle(request, env);
  },
};
