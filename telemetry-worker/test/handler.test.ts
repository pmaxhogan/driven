import { describe, it, expect, vi, afterEach } from "vitest";

import { handle, validatePing, writePing, type Env } from "../src/index";

// Unit tests for the Driven telemetry ingest Worker handler (SPEC s16, M9b).
//
// They drive `handle(request, env)` directly with a MOCKED Analytics Engine
// binding (an object with a `writeDataPoint` spy), so the tests never need the
// Workers runtime and never touch the network. This is the deferred-deploy
// round's static validation of the handler shape + the AE write.

/// Build a mock `Env` whose `TELEMETRY.writeDataPoint` records every data point.
function mockEnv(): { env: Env; writes: unknown[] } {
  const writes: unknown[] = [];
  const env = {
    TELEMETRY: {
      writeDataPoint: (dp: unknown) => {
        writes.push(dp);
      },
    },
  } as unknown as Env;
  return { env, writes };
}

/// A valid SPEC s16 ping payload (matches src-tauri/src/telemetry.rs's wire shape).
function validPayload(): Record<string, unknown> {
  return {
    install_id: "00000000-0000-4000-8000-000000000000",
    ts: 1_700_000_000_000,
    version: "0.1.0",
    os: "windows",
    os_version: null,
    arch: "x86_64",
    channel: "stable",
    events_24h: {
      files_uploaded: 12,
      bytes_uploaded: 345_678,
      errors_by_class: { "drive.rate_limited": 2, "local.io_error": 1 },
      deep_verify_runs: 1,
      update_applied: false,
    },
    latency_p50_p95_ms: {
      scan: [],
      upload_per_mb: [],
    },
  };
}

function postPing(body: string, headers: Record<string, string> = {}): Request {
  return new Request("https://driven.maxhogan.dev/telemetry/v1/ping", {
    method: "POST",
    headers: { "content-type": "application/json", ...headers },
    body,
  });
}

describe("telemetry worker handler", () => {
  it("accepts a valid POST, writes one AE data point, and returns 204", async () => {
    const { env, writes } = mockEnv();
    const res = await handle(postPing(JSON.stringify(validPayload())), env);

    expect(res.status).toBe(204);
    expect(writes).toHaveLength(1);
    const dp = writes[0] as {
      indexes: string[];
      blobs: string[];
      doubles: number[];
    };
    // install_id is the AE index (anonymous sampling key).
    expect(dp.indexes).toEqual(["00000000-0000-4000-8000-000000000000"]);
    // Low-card dims in blobs (os, arch, channel, version, os_version, errors JSON).
    expect(dp.blobs[0]).toBe("windows");
    expect(dp.blobs[1]).toBe("x86_64");
    expect(dp.blobs[2]).toBe("stable");
    expect(dp.blobs[3]).toBe("0.1.0");
    expect(dp.blobs[4]).toBe(""); // os_version null -> ""
    expect(JSON.parse(dp.blobs[5])).toEqual({
      "drive.rate_limited": 2,
      "local.io_error": 1,
    });
    // Numeric measures: files, bytes, deep_verify, update_applied(0/1), total_errors, ts.
    expect(dp.doubles[0]).toBe(12);
    expect(dp.doubles[1]).toBe(345_678);
    expect(dp.doubles[2]).toBe(1);
    expect(dp.doubles[3]).toBe(0); // update_applied false -> 0
    expect(dp.doubles[4]).toBe(3); // total errors = 2 + 1
    expect(dp.doubles[5]).toBe(1_700_000_000_000);
  });

  it("maps update_applied=true to the 0/1 double 1", async () => {
    const { env, writes } = mockEnv();
    const p = validPayload();
    (p.events_24h as Record<string, unknown>).update_applied = true;
    const res = await handle(postPing(JSON.stringify(p)), env);
    expect(res.status).toBe(204);
    const dp = writes[0] as { doubles: number[] };
    expect(dp.doubles[3]).toBe(1);
  });

  it("persists a coarse os_version blob when the client sends one", async () => {
    const { env, writes } = mockEnv();
    const p = validPayload();
    p.os_version = "11.26200";
    const res = await handle(postPing(JSON.stringify(p)), env);
    expect(res.status).toBe(204);
    const dp = writes[0] as { blobs: string[] };
    expect(dp.blobs[4]).toBe("11.26200");
  });

  it("rejects malformed JSON with 400 and writes nothing", async () => {
    const { env, writes } = mockEnv();
    const res = await handle(postPing("{ not json "), env);
    expect(res.status).toBe(400);
    expect(await res.json()).toMatchObject({ error: "invalid_json" });
    expect(writes).toHaveLength(0);
  });

  it("rejects a payload missing a required field with 400", async () => {
    const { env, writes } = mockEnv();
    const bad = validPayload();
    delete (bad as Record<string, unknown>).install_id;
    const res = await handle(postPing(JSON.stringify(bad)), env);
    expect(res.status).toBe(400);
    expect(await res.json()).toMatchObject({ error: "invalid_payload", field: "install_id" });
    expect(writes).toHaveLength(0);
  });

  it("rejects a negative numeric aggregate with 400", async () => {
    const { env, writes } = mockEnv();
    const bad = validPayload();
    (bad.events_24h as Record<string, unknown>).bytes_uploaded = -5;
    const res = await handle(postPing(JSON.stringify(bad)), env);
    expect(res.status).toBe(400);
    expect(await res.json()).toMatchObject({ field: "events_24h.bytes_uploaded" });
    expect(writes).toHaveLength(0);
  });

  it("rejects an oversized body (declared Content-Length) with 400", async () => {
    const { env, writes } = mockEnv();
    const res = await handle(
      postPing(JSON.stringify(validPayload()), { "content-length": String(64 * 1024) }),
      env,
    );
    expect(res.status).toBe(400);
    expect(await res.json()).toMatchObject({ error: "body_too_large" });
    expect(writes).toHaveLength(0);
  });

  it("rejects an oversized actual body with 400 even if the header lies", async () => {
    const { env, writes } = mockEnv();
    const huge = JSON.stringify(validPayload()) + " ".repeat(20 * 1024);
    const res = await handle(postPing(huge), env);
    expect(res.status).toBe(400);
    expect(await res.json()).toMatchObject({ error: "body_too_large" });
    expect(writes).toHaveLength(0);
  });

  it("rejects a >16KiB-BYTE multibyte body even though its UTF-16 length is under the cap", async () => {
    // M9b P2-2: the cap must be BYTE-accurate. A body of 3-byte UTF-8 chars (e.g.
    // CJK) has FEWER UTF-16 code units than bytes, so a `raw.length` (code-unit)
    // cap would wrongly accept a body that is over the byte cap. Build a body that
    // is ~18 KiB of UTF-8 bytes but only ~6000 UTF-16 code units (under 16384), and
    // assert it is rejected as body_too_large with NO AE write.
    const { env, writes } = mockEnv();
    const multibyte = "中".repeat(6000); // 6000 code units, 18000 UTF-8 bytes
    expect(multibyte.length).toBeLessThan(16 * 1024); // under the cap in code units
    expect(new TextEncoder().encode(multibyte).length).toBeGreaterThan(16 * 1024); // over in bytes
    // Either the up-front (byte-measured) Content-Length check or the stream
    // byte-counter rejects it - both are byte-accurate; the point is that the
    // UTF-16 `raw.length` (under the cap) NEVER lets it through. JSON.parse never
    // runs (rejected first).
    const req = new Request("https://driven.maxhogan.dev/telemetry/v1/ping", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: multibyte,
    });
    const res = await handle(req, env);
    expect(res.status).toBe(400);
    expect(await res.json()).toMatchObject({ error: "body_too_large" });
    expect(writes).toHaveLength(0);
  });

  it("rejects an invalid (non-integer) Content-Length up front with 400", async () => {
    // M9b P2-2: a malformed Content-Length is rejected up front rather than
    // trusted. (A negative or NaN length is not a real request.)
    const { env, writes } = mockEnv();
    const res = await handle(
      postPing(JSON.stringify(validPayload()), { "content-length": "not-a-number" }),
      env,
    );
    expect(res.status).toBe(400);
    expect(await res.json()).toMatchObject({ error: "invalid_content_length" });
    expect(writes).toHaveLength(0);
  });

  it("returns 405 for the wrong method on the ping path", async () => {
    const { env, writes } = mockEnv();
    const res = await handle(
      new Request("https://driven.maxhogan.dev/telemetry/v1/ping", { method: "GET" }),
      env,
    );
    expect(res.status).toBe(405);
    expect(res.headers.get("allow")).toBe("POST");
    expect(writes).toHaveLength(0);
  });

  it("returns 404 for any other path", async () => {
    const { env, writes } = mockEnv();
    const res = await handle(
      new Request("https://driven.maxhogan.dev/telemetry/v2/ping", { method: "POST" }),
      env,
    );
    expect(res.status).toBe(404);
    expect(writes).toHaveLength(0);
  });

  it("returns 5xx (not 204) if the AE write throws, so the client does not checkpoint", async () => {
    // M9b P2-3: a throwing writeDataPoint (e.g. a misconfigured / missing AE
    // binding) must NOT 204 - a 204 would make the client advance last_sent_at and
    // permanently drop the window. A 5xx makes the client skip the checkpoint and
    // retry on the next scheduled tick (no retry-storm).
    const env = {
      TELEMETRY: {
        writeDataPoint: () => {
          throw new Error("AE unavailable");
        },
      },
    } as unknown as Env;
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const res = await handle(postPing(JSON.stringify(validPayload())), env);
    expect(res.status).toBeGreaterThanOrEqual(500);
    expect(res.status).toBeLessThan(600);
    expect(await res.json()).toMatchObject({ error: "write_failed" });
    // It logged a GENERIC message (no payload/body).
    expect(errSpy).toHaveBeenCalledWith("telemetry: writeDataPoint failed");
    errSpy.mockRestore();
  });

  it("validatePing accepts empty latency arrays (V1) and a missing os_version", () => {
    const p = validPayload();
    delete (p as Record<string, unknown>).os_version;
    const r = validatePing(p);
    expect(r.ok).toBe(true);
    if (r.ok) {
      expect(r.payload.os_version).toBeNull();
      expect(r.payload.latency_p50_p95_ms.scan).toEqual([]);
    }
  });

  it("writePing maps the payload onto the AE data point schema", () => {
    const { env, writes } = mockEnv();
    const r = validatePing(validPayload());
    expect(r.ok).toBe(true);
    if (r.ok) writePing(env, r.payload);
    expect(writes).toHaveLength(1);
  });

  // ----------------------------------------------------------------------
  // PUBLIC-ENDPOINT HARDENING (M9b P1-1): reject PII / junk before AE.
  // ----------------------------------------------------------------------

  it("rejects a path-shaped install_id with 400 (not a UUID v4)", async () => {
    const { env, writes } = mockEnv();
    const bad = validPayload();
    bad.install_id = "C:/Users/alice/Documents/secret.txt";
    const res = await handle(postPing(JSON.stringify(bad)), env);
    expect(res.status).toBe(400);
    expect(await res.json()).toMatchObject({ error: "invalid_payload", field: "install_id" });
    expect(writes).toHaveLength(0);
  });

  it("rejects an email-shaped install_id with 400 (not a UUID v4)", async () => {
    const { env, writes } = mockEnv();
    const bad = validPayload();
    bad.install_id = "alice@example.com";
    const res = await handle(postPing(JSON.stringify(bad)), env);
    expect(res.status).toBe(400);
    expect(await res.json()).toMatchObject({ field: "install_id" });
    expect(writes).toHaveLength(0);
  });

  it("rejects a non-v4 UUID (wrong version nibble) with 400", async () => {
    const bad = validPayload();
    // A v1-style UUID (version nibble 1) must be rejected.
    bad.install_id = "00000000-0000-1000-8000-000000000000";
    const r = validatePing(bad);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toBe("install_id");
  });

  it("rejects a junk channel not in the whitelist with 400", async () => {
    const { env, writes } = mockEnv();
    const bad = validPayload();
    bad.channel = "beta'; DROP TABLE--";
    const res = await handle(postPing(JSON.stringify(bad)), env);
    expect(res.status).toBe(400);
    expect(await res.json()).toMatchObject({ field: "channel" });
    expect(writes).toHaveLength(0);
  });

  it("rejects a junk os not in the whitelist with 400", async () => {
    const bad = validPayload();
    bad.os = "/etc/passwd";
    const r = validatePing(bad);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toBe("os");
  });

  it("rejects a junk arch not in the whitelist with 400", async () => {
    const bad = validPayload();
    bad.arch = "i-made-this-up";
    const r = validatePing(bad);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toBe("arch");
  });

  it("rejects an errors_by_class key outside the SPEC s24 code set with 400", async () => {
    const { env, writes } = mockEnv();
    const bad = validPayload();
    (bad.events_24h as Record<string, unknown>).errors_by_class = {
      "drive.rate_limited": 1,
      "C:/Users/alice/secret.txt": 5, // path-shaped junk key
    };
    const res = await handle(postPing(JSON.stringify(bad)), env);
    expect(res.status).toBe(400);
    const body = (await res.json()) as { error: string; field: string };
    expect(body.error).toBe("invalid_payload");
    expect(body.field).toContain("errors_by_class");
    expect(writes).toHaveLength(0);
  });

  it("rejects an email-shaped errors_by_class key with 400", async () => {
    const bad = validPayload();
    (bad.events_24h as Record<string, unknown>).errors_by_class = {
      "alice@example.com": 1,
    };
    const r = validatePing(bad);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toContain("errors_by_class");
  });

  it("rejects too many errors_by_class keys (high-cardinality flood) with 400", async () => {
    const bad = validPayload();
    const flood: Record<string, number> = {};
    for (let i = 0; i < 200; i++) flood[`internal.bug${i}`] = 1;
    (bad.events_24h as Record<string, unknown>).errors_by_class = flood;
    const r = validatePing(bad);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toContain("errors_by_class");
  });

  it("rejects a non-integer / out-of-range errors_by_class value with 400", async () => {
    const bad = validPayload();
    (bad.events_24h as Record<string, unknown>).errors_by_class = {
      "drive.rate_limited": 1e15, // absurdly large -> over MAX_ERROR_COUNT
    };
    const r = validatePing(bad);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toContain("errors_by_class");
  });

  it("rejects an over-long version string with 400", async () => {
    const bad = validPayload();
    bad.version = "9".repeat(128);
    const r = validatePing(bad);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toBe("version");
  });

  it("rejects an over-long / path-shaped os_version with 400", async () => {
    const bad = validPayload();
    bad.os_version = "/" + "a".repeat(128);
    const r = validatePing(bad);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toBe("os_version");
  });

  it("rejects a numeric update_applied (must be boolean) with 400", async () => {
    const bad = validPayload();
    (bad.events_24h as Record<string, unknown>).update_applied = 1;
    const r = validatePing(bad);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toBe("events_24h.update_applied");
  });

  it("accepts a valid lowercase UUID v4 install_id", () => {
    const p = validPayload();
    p.install_id = "9f8e7d6c-5b4a-4392-8170-0a1b2c3d4e5f";
    const r = validatePing(p);
    expect(r.ok).toBe(true);
  });

  // ----------------------------------------------------------------------
  // M9b P1-2: content-validate version + os_version (not just length), and
  // verify the regexes ACCEPT the client's REAL emitted shapes (telemetry.rs).
  // ----------------------------------------------------------------------

  it("accepts the client's real stable + dev version shapes (semver / prerelease)", () => {
    // telemetry.rs reports AppHandle::package_info().version: a plain semver on
    // the stable channel, and `0.1.1-dev.<run>.<sha>` on the CI dev channel.
    for (const version of [
      "0.1.0",
      "1.2.3",
      "10.20.30",
      "0.1.1-dev.123.ab0c9f1",
      "1.0.0-rc.1",
      "1.0.0+build.5",
      "0.1.1-dev.123.ab0c9f1+meta",
    ]) {
      const p = validPayload();
      p.version = version;
      const r = validatePing(p);
      expect(r.ok, `version ${version} must be accepted`).toBe(true);
    }
  });

  it("accepts the client's real coarse os_version shapes (os_info)", () => {
    // coarse_os_version() renders an os_info Version: dotted numeric builds on the
    // major platforms, or a short Custom string (codename / distro release).
    for (const osv of ["11.26200", "14.5", "10.0.19045", "22.04 LTS", "rolling", "Sonoma"]) {
      const p = validPayload();
      p.os_version = osv;
      const r = validatePing(p);
      expect(r.ok, `os_version ${osv} must be accepted`).toBe(true);
    }
  });

  it("rejects an email-shaped version with 400 (PII, even though it is short)", async () => {
    const { env, writes } = mockEnv();
    const bad = validPayload();
    bad.version = "alice@example.com";
    const res = await handle(postPing(JSON.stringify(bad)), env);
    expect(res.status).toBe(400);
    expect(await res.json()).toMatchObject({ error: "invalid_payload", field: "version" });
    expect(writes).toHaveLength(0);
  });

  it("rejects a path-shaped version with 400 (PII, short)", () => {
    const bad = validPayload();
    bad.version = "/home/alice";
    const r = validatePing(bad);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toBe("version");
  });

  it("rejects a Windows-path-shaped version with 400 (backslash)", () => {
    const bad = validPayload();
    bad.version = "C:\\Users\\alice";
    const r = validatePing(bad);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toBe("version");
  });

  it("rejects an email-shaped os_version with 400 (PII, short)", async () => {
    const { env, writes } = mockEnv();
    const bad = validPayload();
    bad.os_version = "alice@example.com";
    const res = await handle(postPing(JSON.stringify(bad)), env);
    expect(res.status).toBe(400);
    expect(await res.json()).toMatchObject({ error: "invalid_payload", field: "os_version" });
    expect(writes).toHaveLength(0);
  });

  it("rejects a path-shaped os_version with 400 (PII, short)", () => {
    const bad = validPayload();
    bad.os_version = "/home/alice";
    const r = validatePing(bad);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toBe("os_version");
  });

  it("rejects an os_version with a whitespace-run (padding) with 400", () => {
    const bad = validPayload();
    bad.os_version = "11.26200  hidden";
    const r = validatePing(bad);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toBe("os_version");
  });

  // ----------------------------------------------------------------------
  // M9b P2-2: integer + range validation on numeric fields (reject fractions,
  // huge finite doubles, and an absurd ts).
  // ----------------------------------------------------------------------

  it("rejects a fractional files_uploaded with 400", () => {
    const bad = validPayload();
    (bad.events_24h as Record<string, unknown>).files_uploaded = 1.5;
    const r = validatePing(bad);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toBe("events_24h.files_uploaded");
  });

  it("rejects a fractional bytes_uploaded with 400", () => {
    const bad = validPayload();
    (bad.events_24h as Record<string, unknown>).bytes_uploaded = 100.0001;
    const r = validatePing(bad);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toBe("events_24h.bytes_uploaded");
  });

  it("rejects a huge (non-safe-integer) bytes_uploaded with 400", async () => {
    const { env, writes } = mockEnv();
    const bad = validPayload();
    // Above Number.MAX_SAFE_INTEGER -> not a safe integer, must be rejected.
    (bad.events_24h as Record<string, unknown>).bytes_uploaded = 1e20;
    const res = await handle(postPing(JSON.stringify(bad)), env);
    expect(res.status).toBe(400);
    expect(await res.json()).toMatchObject({ field: "events_24h.bytes_uploaded" });
    expect(writes).toHaveLength(0);
  });

  it("rejects a bytes_uploaded over the per-field cap with 400", () => {
    const bad = validPayload();
    // A safe integer, but above the 1 PiB/day cap.
    (bad.events_24h as Record<string, unknown>).bytes_uploaded = 1_125_899_906_842_625;
    const r = validatePing(bad);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toBe("events_24h.bytes_uploaded");
  });

  it("rejects a fractional deep_verify_runs with 400", () => {
    const bad = validPayload();
    (bad.events_24h as Record<string, unknown>).deep_verify_runs = 0.5;
    const r = validatePing(bad);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toBe("events_24h.deep_verify_runs");
  });

  it("rejects a fractional ts with 400", () => {
    const bad = validPayload();
    bad.ts = 1_700_000_000_000.5;
    const r = validatePing(bad);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toBe("ts");
  });

  it("rejects an absurd far-future ts with 400", async () => {
    const { env, writes } = mockEnv();
    const bad = validPayload();
    bad.ts = 99_999_999_999_999; // year ~5138
    const res = await handle(postPing(JSON.stringify(bad)), env);
    expect(res.status).toBe(400);
    expect(await res.json()).toMatchObject({ field: "ts" });
    expect(writes).toHaveLength(0);
  });

  it("rejects a seconds-granularity ts (too small) with 400", () => {
    const bad = validPayload();
    bad.ts = 1_700_000_000; // seconds, not ms -> below TS_MIN_MS
    const r = validatePing(bad);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toBe("ts");
  });

  it("rejects a huge (non-safe-integer) ts with 400", () => {
    const bad = validPayload();
    bad.ts = 1e30;
    const r = validatePing(bad);
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.reason).toBe("ts");
  });
});

// --------------------------------------------------------------------------
// DESIGN s13: latency percentile doubles + the gated /stats/latency rollup.
// --------------------------------------------------------------------------

/// The AE double indices (0-based) the 4 latency percentiles occupy - appended
/// after the original 6 measures (files, bytes, deep_verify, update_applied,
/// total_errors, ts).
const SCAN_P50 = 6;
const SCAN_P95 = 7;
const UP_P50 = 8;
const UP_P95 = 9;

describe("telemetry worker latency doubles (DESIGN s13)", () => {
  it("writes the client-reported [p50, p95] latency doubles", async () => {
    const { env, writes } = mockEnv();
    const p = validPayload();
    p.latency_p50_p95_ms = { scan: [3, 12], upload_per_mb: [40, 110] };
    const res = await handle(postPing(JSON.stringify(p)), env);
    expect(res.status).toBe(204);
    const dp = writes[0] as { doubles: number[] };
    // The original 6 measures keep their positions.
    expect(dp.doubles[5]).toBe(1_700_000_000_000); // ts unchanged at double6
    expect(dp.doubles[SCAN_P50]).toBe(3);
    expect(dp.doubles[SCAN_P95]).toBe(12);
    expect(dp.doubles[UP_P50]).toBe(40);
    expect(dp.doubles[UP_P95]).toBe(110);
  });

  it("writes the -1 sentinel for a metric with no samples (empty array)", async () => {
    const { env, writes } = mockEnv();
    const p = validPayload();
    // Scan has data; upload had no completed uploads this window (empty array).
    p.latency_p50_p95_ms = { scan: [0, 5], upload_per_mb: [] };
    const res = await handle(postPing(JSON.stringify(p)), env);
    expect(res.status).toBe(204);
    const dp = writes[0] as { doubles: number[] };
    // A legit 0 ms p50 is preserved (NOT turned into the sentinel).
    expect(dp.doubles[SCAN_P50]).toBe(0);
    expect(dp.doubles[SCAN_P95]).toBe(5);
    // The empty upload metric -> -1 sentinel (distinguishable from a real 0).
    expect(dp.doubles[UP_P50]).toBe(-1);
    expect(dp.doubles[UP_P95]).toBe(-1);
  });

  it("writes both sentinels when V1-style empty latency arrays arrive", async () => {
    const { env, writes } = mockEnv();
    // The default validPayload() carries empty latency arrays (the V1 wire shape).
    const res = await handle(postPing(JSON.stringify(validPayload())), env);
    expect(res.status).toBe(204);
    const dp = writes[0] as { doubles: number[] };
    expect(dp.doubles[SCAN_P50]).toBe(-1);
    expect(dp.doubles[SCAN_P95]).toBe(-1);
    expect(dp.doubles[UP_P50]).toBe(-1);
    expect(dp.doubles[UP_P95]).toBe(-1);
  });
});

describe("telemetry worker GET /stats/latency (DESIGN s13)", () => {
  const STATS_URL = "https://driven.maxhogan.dev/telemetry/v1/stats/latency";

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  /// A configured stats Env (both secrets present) plus a fetch stub returning
  /// the given per-metric AE `{ data }` rows for the two metric queries in order.
  function statsEnv(): Env {
    return {
      TELEMETRY: { writeDataPoint: () => undefined },
      QUERY_TOKEN: "s3cret",
      CF_API_TOKEN: "cf-token",
      CF_ACCOUNT_ID: "acct-123",
    } as unknown as Env;
  }

  function authed(): Request {
    return new Request(STATS_URL, {
      method: "GET",
      headers: { authorization: "Bearer s3cret" },
    });
  }

  it("503s when QUERY_TOKEN / CF_API_TOKEN are not configured", async () => {
    const env = { TELEMETRY: { writeDataPoint: () => undefined } } as unknown as Env;
    const res = await handle(authed(), env);
    expect(res.status).toBe(503);
    expect(await res.json()).toMatchObject({ error: "stats_not_configured" });
  });

  it("401s without a bearer token", async () => {
    const res = await handle(new Request(STATS_URL, { method: "GET" }), statsEnv());
    expect(res.status).toBe(401);
    expect(res.headers.get("www-authenticate")).toBe("Bearer");
  });

  it("401s with the wrong bearer token", async () => {
    const req = new Request(STATS_URL, {
      method: "GET",
      headers: { authorization: "Bearer nope" },
    });
    const res = await handle(req, statsEnv());
    expect(res.status).toBe(401);
  });

  it("405s on the wrong method for the stats path", async () => {
    const res = await handle(new Request(STATS_URL, { method: "POST" }), statsEnv());
    expect(res.status).toBe(405);
    expect(res.headers.get("allow")).toBe("GET");
  });

  it("returns per-day aggregates for both metrics on a valid authed request", async () => {
    const sqls: string[] = [];
    const fetchMock = vi.fn(async (_url: string, init: { body: string }) => {
      sqls.push(init.body);
      // First call (scan) then second (upload); return distinct rows.
      const isScan = init.body.includes("double7");
      const data = isScan
        ? [{ day: "2026-07-14", avg_p50: "3", avg_p95: "12", max_p95: "40", samples: "9" }]
        : [{ day: "2026-07-14", avg_p50: "50", avg_p95: "120", max_p95: "300", samples: "4" }];
      return new Response(JSON.stringify({ meta: [], data }), { status: 200 });
    });
    vi.stubGlobal("fetch", fetchMock);

    const res = await handle(authed(), statsEnv());
    expect(res.status).toBe(200);
    const body = (await res.json()) as {
      days: number;
      metrics: { scan: unknown[]; upload_per_mb: unknown[] };
    };
    expect(body.days).toBe(7); // default window
    expect(body.metrics.scan).toEqual([
      { day: "2026-07-14", avg_p50_ms: 3, avg_p95_ms: 12, max_p95_ms: 40, samples: 9 },
    ]);
    expect(body.metrics.upload_per_mb).toEqual([
      { day: "2026-07-14", avg_p50_ms: 50, avg_p95_ms: 120, max_p95_ms: 300, samples: 4 },
    ]);
    // Two queries issued, one per metric, each filtering its own sentinel column.
    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(sqls.some((s) => s.includes("double7") && s.includes("double7 >= 0"))).toBe(true);
    expect(sqls.some((s) => s.includes("double9") && s.includes("double9 >= 0"))).toBe(true);
    // The default 7-day window is in the SQL.
    expect(sqls.every((s) => s.includes("INTERVAL '7' DAY"))).toBe(true);
  });

  it("clamps the days param to [1, 90] and defaults to 7", async () => {
    const seen: string[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (_url: string, init: { body: string }) => {
        seen.push(init.body);
        return new Response(JSON.stringify({ meta: [], data: [] }), { status: 200 });
      }),
    );
    const call = async (q: string) =>
      handle(
        new Request(`${STATS_URL}?days=${q}`, {
          method: "GET",
          headers: { authorization: "Bearer s3cret" },
        }),
        statsEnv(),
      );

    await call("500"); // over the cap -> 90
    await call("0"); // under the floor -> 1
    await call("abc"); // non-numeric -> default 7
    await call("30"); // valid pass-through

    expect(seen.some((s) => s.includes("INTERVAL '90' DAY"))).toBe(true);
    expect(seen.some((s) => s.includes("INTERVAL '1' DAY"))).toBe(true);
    expect(seen.some((s) => s.includes("INTERVAL '7' DAY"))).toBe(true);
    expect(seen.some((s) => s.includes("INTERVAL '30' DAY"))).toBe(true);
  });

  it("502s (not 200) when the AE SQL query fails upstream", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response("nope", { status: 403 })),
    );
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const res = await handle(authed(), statsEnv());
    expect(res.status).toBe(502);
    expect(await res.json()).toMatchObject({ error: "query_failed" });
    errSpy.mockRestore();
  });
});
