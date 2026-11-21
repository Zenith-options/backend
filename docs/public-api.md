# Public transparency API

`/api/v1/public/*` serves only precomputed protocol aggregates for daily volume, open interest, P&L distributions, strategy popularity, settlements, insurance fund history, governance participation, treasury, fees, and DefiLlama-compatible TVL/volume snapshots.

Buckets representing fewer than ten wallets are suppressed. Time windows are coarse and fixed to reduce differencing risk. Responses include an ETag and `Cache-Control: public, max-age=300, stale-while-revalidate=60`; breaking field changes require a new API version.

## Timestamps

Every timestamp field is returned twice: the original integer epoch (seconds since the Unix epoch, kept for backward compatibility) and an ISO-8601 companion named `<field>_iso`. The companion is always UTC and ends with a `Z` suffix, so it can be announced directly by screen readers and parsed by clients without extra formatting.

For example, a response containing `"updated_at": 1700000000` also contains `"updated_at_iso": "2023-11-14T22:13:20Z"`. Both values always refer to the same instant; clients may use either, but new integrations should prefer the `_iso` string.
