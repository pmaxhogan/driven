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
      update_applied: 0,
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
    // Low-card dimensions in blobs (os, arch, channel, version, errors JSON).
    expect(dp.blobs[0]).toBe("windows");
    expect(dp.blobs[1]).toBe("x86_64");
    expect(dp.blobs[2]).toBe("stable");
    expect(dp.blobs[3]).toBe("0.1.0");
    expect(JSON.parse(dp.blobs[4])).toEqual({
      "drive.rate_limited": 2,
      "local.io_error": 1,
    });
    // Numeric measures: files, bytes, deep_verify, update_applied, total_errors, ts.
    expect(dp.doubles[0]).toBe(12);
    expect(dp.doubles[1]).toBe(345_678);
    expect(dp.doubles[2]).toBe(1);
    expect(dp.doubles[3]).toBe(0);
    expect(dp.doubles[4]).toBe(3); // total errors = 2 + 1
    expect(dp.doubles[5]).toBe(1_700_000_000_000);
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
});
