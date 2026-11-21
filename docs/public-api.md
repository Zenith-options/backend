# Public transparency API

`/api/v1/public/*` serves only precomputed protocol aggregates for daily volume, open interest, P&L distributions, strategy popularity, settlements, insurance fund history, governance participation, treasury, fees, and DefiLlama-compatible TVL/volume snapshots.

Buckets representing fewer than ten wallets are suppressed. Time windows are coarse and fixed to reduce differencing risk. Responses include an ETag and `Cache-Control: public, max-age=300, stale-while-revalidate=60`; breaking field changes require a new API version.

## Health endpoint

`GET /health` reports service liveness plus database connection pool statistics so pool saturation is visible before requests start timing out. The response body is additive: the HTTP status logic is unchanged (up/down only), and the pool fields are informational.

```json
{
  "status": "ok",
  "pool": {
    "size": 20,
    "idle": 14,
    "inUse": 6,
    "waiters": 0
  }
}
```

- `pool.size` — total connections in the pool.
- `pool.idle` — connections currently idle and available.
- `pool.inUse` — connections currently checked out.
- `pool.waiters` — requests queued waiting for a connection; a sustained non-zero value indicates saturation.

All pool fields are numeric. The shape is reflected in `openapi/openapi.yaml`.
