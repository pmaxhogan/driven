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
  os_version?: string | null;
  arch: string;
  channel: string;
  events_24h: {
    files_uploaded: number;
    bytes_uploaded: number;
    errors_by_class: Record<string, number>;
    deep_verify_runs: number;
    update_applied: number;
  };
  latency_p50_p95_ms: {
    scan: number[];
    upload_per_mb: number[];
  };
}

/// A small JSON Response helper that never echoes the request body.
function json(status: number, body: Record<string, unknown>): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

/// Type guard: is `v` a finite, non-negative number? (counts/sizes must be.)
function isNonNegNumber(v: unknown): v is number {
  return typeof v === "number" && Number.isFinite(v) && v >= 0;
}

/// Type guard: is `v` a non-empty string?
function isNonEmptyString(v: unknown): v is string {
  return typeof v === "string" && v.length > 0;
}

/// Validate the parsed JSON into a [`PingPayload`], or return a reason string for
/// a 400. Strict on the load-bearing fields (install_id, version, os, arch,
/// channel, the numeric aggregates); tolerant of an absent `os_version` and of an
/// empty latency array (V1 sends empty arrays). Never throws.
export function validatePing(value: unknown): { ok: true; payload: PingPayload } | { ok: false; reason: string } {
  if (typeof value !== "object" || value === null) {
    return { ok: false, reason: "body is not a JSON object" };
  }
  const v = value as Record<string, unknown>;

  if (!isNonEmptyString(v.install_id)) return { ok: false, reason: "install_id" };
  if (!isNonNegNumber(v.ts)) return { ok: false, reason: "ts" };
  if (!isNonEmptyString(v.version)) return { ok: false, reason: "version" };
  if (!isNonEmptyString(v.os)) return { ok: false, reason: "os" };
  if (!isNonEmptyString(v.arch)) return { ok: false, reason: "arch" };
  if (!isNonEmptyString(v.channel)) return { ok: false, reason: "channel" };

  const e = v.events_24h;
  if (typeof e !== "object" || e === null) return { ok: false, reason: "events_24h" };
  const ev = e as Record<string, unknown>;
  if (!isNonNegNumber(ev.files_uploaded)) return { ok: false, reason: "events_24h.files_uploaded" };
  if (!isNonNegNumber(ev.bytes_uploaded)) return { ok: false, reason: "events_24h.bytes_uploaded" };
  if (!isNonNegNumber(ev.deep_verify_runs)) return { ok: false, reason: "events_24h.deep_verify_runs" };
  if (!isNonNegNumber(ev.update_applied)) return { ok: false, reason: "events_24h.update_applied" };
  if (typeof ev.errors_by_class !== "object" || ev.errors_by_class === null) {
    return { ok: false, reason: "events_24h.errors_by_class" };
  }
  // errors_by_class values must all be non-negative numbers (counts).
  for (const [k, n] of Object.entries(ev.errors_by_class as Record<string, unknown>)) {
    if (!isNonNegNumber(n)) return { ok: false, reason: `events_24h.errors_by_class[${k}]` };
  }

  const l = v.latency_p50_p95_ms;
  if (typeof l !== "object" || l === null) return { ok: false, reason: "latency_p50_p95_ms" };
  const lv = l as Record<string, unknown>;
  if (!Array.isArray(lv.scan)) return { ok: false, reason: "latency_p50_p95_ms.scan" };
  if (!Array.isArray(lv.upload_per_mb)) return { ok: false, reason: "latency_p50_p95_ms.upload_per_mb" };

  return {
    ok: true,
    payload: {
      install_id: v.install_id as string,
      ts: v.ts as number,
      version: v.version as string,
      os: v.os as string,
      os_version: typeof v.os_version === "string" ? (v.os_version as string) : null,
      arch: v.arch as string,
      channel: v.channel as string,
      events_24h: {
        files_uploaded: ev.files_uploaded as number,
        bytes_uploaded: ev.bytes_uploaded as number,
        errors_by_class: ev.errors_by_class as Record<string, number>,
        deep_verify_runs: ev.deep_verify_runs as number,
        update_applied: ev.update_applied as number,
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
/// - blobs:   [os, arch, channel, version, errors_by_class JSON]  (low-card dims)
/// - doubles: [files_uploaded, bytes_uploaded, deep_verify_runs, update_applied,
///             total_errors, ts]  (the numeric measures)
/// Writes are non-blocking (no await / waitUntil needed per the CF docs).
export function writePing(env: Env, p: PingPayload): void {
  env.TELEMETRY.writeDataPoint({
    indexes: [p.install_id],
    blobs: [
      p.os,
      p.arch,
      p.channel,
      p.version,
      // The error-code -> count map as JSON (codes are a fixed enum; never PII).
      JSON.stringify(p.events_24h.errors_by_class),
    ],
    doubles: [
      p.events_24h.files_uploaded,
      p.events_24h.bytes_uploaded,
      p.events_24h.deep_verify_runs,
      p.events_24h.update_applied,
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
