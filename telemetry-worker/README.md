# Driven telemetry Worker

The server side of Driven's opt-out anonymous telemetry (DESIGN s13, SPEC s16). A
small Cloudflare Worker that:

- **Ingests** the usage ping the desktop client POSTs on startup + every 24h
  (`POST /telemetry/v1/ping`), validates it strictly (public endpoint - never
  trust the client), and writes it to an Analytics Engine dataset.
- **Serves a gated latency rollup** (`GET /telemetry/v1/stats/latency`) reading
  the per-day scan / upload-per-MB percentiles back out via the Analytics Engine
  SQL API.

It is deployed to `driven.maxhogan.dev/telemetry/*` (the Worker route takes
precedence over the CF Pages site for that prefix) and auto-deploys via
`.github/workflows/deploy-telemetry.yml` on any change under `telemetry-worker/`.

## Toolchain

Its own toolchain, NOT part of the cargo workspace or the `ui/` build.

```sh
pnpm install
pnpm run typecheck   # tsc --noEmit
pnpm run lint        # eslint
pnpm test            # vitest (handler unit tests, mocked AE + fetch)
pnpm run deploy      # wrangler deploy (CI does this on merge)
```

## Analytics Engine dataset layout (`driven_telemetry`)

One data point per ping (`writePing`):

| column | value |
|---|---|
| `index1` | `install_id` (anonymous UUID v4 sampling key) |
| `blob1..6` | `os`, `arch`, `channel`, `version`, `os_version` (`""` if absent), `errors_by_class` JSON |
| `double1..6` | `files_uploaded`, `bytes_uploaded`, `deep_verify_runs`, `update_applied` (0/1), `total_errors`, `ts` (epoch ms) |
| `double7..10` | `scan_p50`, `scan_p95`, `upload_per_mb_p50`, `upload_per_mb_p95` (ms) |
| `double11` | `latency_schema_version` (schema marker, `1`) |

The latency doubles (DESIGN s13) are **appended** so the original columns keep
their positions. When the client had no samples for a metric this window (its
array is empty), the pair is written as the sentinel **`-1`** so the rollup query
can tell "no samples" apart from a legitimate `0 ms` (a sub-millisecond per-file
scan rounds to 0).

`double11` is a **schema marker** (`1`) written on every row that carries the
latency doubles. It exists because the Analytics Engine SQL API has no NULLs and
materializes any double a row never wrote as `0`: rows written by the pre-latency
Worker have `scan_p50 == 0` (a materialized 0, not a real sample) and would
otherwise pass the `>= 0` sentinel filter and pollute the rollup as fake `0 ms`
samples. The rollup filters `WHERE double11 >= 1`, so pre-latency rows (marker
materializes as `0`) are excluded.

## `GET /telemetry/v1/stats/latency`

Per-day aggregates of the client-reported percentiles. **Authenticated** - it
exposes aggregate telemetry, so it is never served open.

- **Auth:** `Authorization: Bearer <QUERY_TOKEN>`. A missing/wrong token is `401`.
- **Query:** `?days=N` - lookback window, default `7`, clamped to `[1, 90]`.
- **Contract:**

```
GET /telemetry/v1/stats/latency?days=7
Authorization: Bearer <QUERY_TOKEN>

200 OK
{
  "days": 7,
  "metrics": {
    "scan":          [ { "day": "2026-07-14", "avg_p50_ms": 3,  "avg_p95_ms": 12,  "max_p95_ms": 40,  "samples": 9 } ],
    "upload_per_mb": [ { "day": "2026-07-14", "avg_p50_ms": 50, "avg_p95_ms": 120, "max_p95_ms": 300, "samples": 4 } ]
  }
}
```

Per metric, per UTC day: `avg_p50_ms` (mean of the pinged p50s), `avg_p95_ms`
(mean of the pinged p95s), `max_p95_ms` (worst pinged p95), and `samples` (number
of pings that reported the metric). Each metric query excludes two kinds of
non-sample rows (`WHERE double11 >= 1 AND <p50col> >= 0`): pre-latency rows (the
schema marker materializes as `0`) and empty-latency pings (the `-1` sentinel),
while keeping a real `0 ms`. The two metrics are queried separately (each filters
its own sentinel column) via the Analytics Engine SQL API.

Status codes: `200` success; `401` missing/wrong bearer; `405` non-GET;
`502` upstream AE SQL query failed; `503` the endpoint is not configured
(a required secret is missing - see below).

### Note on the documented path

The task refers to this as `GET /stats/latency`. The Worker route only serves the
`/telemetry/*` prefix (`wrangler.jsonc`), so the real path is
`/telemetry/v1/stats/latency`.

## Required secrets / vars (set post-deploy)

The ingest path needs none of these; the `/stats/latency` READ path needs all
three. Until they are set, the endpoint returns `503 stats_not_configured` (it
never falls through to an unauthenticated or broken read). AE **reads** go through
the SQL HTTP API (the write binding cannot read), which needs a Cloudflare API
token - hence `CF_API_TOKEN`.

| name | kind | purpose |
|---|---|---|
| `QUERY_TOKEN` | secret | Bearer token gating `/stats/latency`. Generate a random value. |
| `CF_API_TOKEN` | secret | Cloudflare API token with **Account Analytics: Read** on the Driven account, used to call the AE SQL API. |
| `CF_ACCOUNT_ID` | var (optional) | Account id for the SQL API URL. Defaults to the Driven account (`9c20c14daa20466a2d761a47162f719a`) when unset. |

```sh
npx wrangler secret put QUERY_TOKEN
npx wrangler secret put CF_API_TOKEN
# optional (defaults to the Driven account):
npx wrangler secret put CF_ACCOUNT_ID   # or set as a [vars] entry
```

> The `/stats/latency` endpoint's SQL was validated in unit tests against a mocked
> `fetch`; the live query (day-grouping function, response shape) should be
> smoke-checked against the real Analytics Engine SQL API once the secrets are set.
