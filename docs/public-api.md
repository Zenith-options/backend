# Public transparency API

`/api/v1/public/*` serves only precomputed protocol aggregates for daily volume, open interest, P&L distributions, strategy popularity, settlements, insurance fund history, governance participation, treasury, fees, and DefiLlama-compatible TVL/volume snapshots.

Buckets representing fewer than ten wallets are suppressed. Time windows are coarse and fixed to reduce differencing risk. Responses include an ETag and `Cache-Control: public, max-age=300, stale-while-revalidate=60`; breaking field changes require a new API version.

## Versioning

All public endpoints are served under the `/v1/` prefix (e.g. `/api/v1/public/volume`). The version is part of the URL path; there is no unversioned alias. Breaking field changes ship under a new version prefix rather than mutating `/v1/`. See `docs/api-versioning-policy.md` for the full policy, including the deprecation header (`Deprecation` / `Sunset`) returned on endpoints scheduled for removal.

## Pagination

List endpoints accept `limit` and `offset` query parameters. Both must be integers within the following ranges:

- `limit` — `1` to `100` (inclusive). Defaults to `20` when omitted.
- `offset` — `0` to `1000000` (inclusive). Defaults to `0` when omitted.

Non-integer values (e.g. `?limit=abc`), negative values (e.g. `?limit=-1`), and values outside the ranges above (e.g. `?offset=99999999999999999999`) are rejected with `400 Bad Request` using the standard error envelope and the `invalid_pagination` error code. See `docs/error-codes.json` for the full catalogue.

## Health and readiness

The service exposes two distinct operational endpoints. They answer different questions and are wired to different Kubernetes probes.

### `GET /api/v1/health` — liveness

Guarantees only that the process is alive and able to serve HTTP. It does **not** touch the database or any other dependency.

- `200 OK` — the process is up.
- Any other status (or a timeout) means the process is wedged and should be restarted.

Use this for the `livenessProbe`. A failing database must not cause a restart loop, so `/api/v1/health` stays dependency-free.

### `GET /api/v1/ready` — readiness

Guarantees that the process is alive **and** the database is reachable. It performs a live database probe on every call.

- `200 OK` — the process is up and the database is reachable; the instance can serve traffic.
- `503 Service Unavailable` — the process is up but the database is unreachable (pool unavailable or probe failed); the instance should be removed from load balancing until it recovers.

Use this for the `readinessProbe`. Because readiness reflects dependency health, a pod can be alive (`/api/v1/health` = 200) while not ready (`/api/v1/ready` = 503) during a database outage.
