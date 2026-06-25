import { describe, it, expect, vi } from "vitest";

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

  it("still returns 204 if the AE write throws (best-effort, no client retry-storm)", async () => {
    const env = {
      TELEMETRY: {
        writeDataPoint: () => {
          throw new Error("AE unavailable");
        },
      },
    } as unknown as Env;
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const res = await handle(postPing(JSON.stringify(validPayload())), env);
    expect(res.status).toBe(204);
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
