# Public transparency API

`/api/v1/public/*` serves only precomputed protocol aggregates for daily volume, open interest, P&L distributions, strategy popularity, settlements, insurance fund history, governance participation, treasury, fees, and DefiLlama-compatible TVL/volume snapshots.

Buckets representing fewer than ten wallets are suppressed. Time windows are coarse and fixed to reduce differencing risk. Responses include an ETag and `Cache-Control: public, max-age=300, stale-while-revalidate=60`; breaking field changes require a new API version.
