# Public transparency API

`/api/v1/public/*` serves only precomputed protocol aggregates for daily volume, open interest, P&L distributions, strategy popularity, settlements, insurance fund history, governance participation, treasury, fees, and DefiLlama-compatible TVL/volume snapshots.

Buckets representing fewer than ten wallets are suppressed. Time windows are coarse and fixed to reduce differencing risk. Responses include an ETag and `Cache-Control: public, max-age=300, stale-while-revalidate=60`; breaking field changes require a new API version.

## Rate limiting

Rate limiting is enforced at the edge, not by the API server. The server does not emit `X-RateLimit-Remaining` or `X-RateLimit-Reset` headers, so clients must not rely on them for backoff. Consult the edge/proxy configuration under `deploy/` for the actual limits and throttling behaviour.
