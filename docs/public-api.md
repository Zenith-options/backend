# Public transparency API

`/api/v1/public/*` serves only precomputed protocol aggregates for daily volume, open interest, P&L distributions, strategy popularity, settlements, insurance fund history, governance participation, treasury, fees, and DefiLlama-compatible TVL/volume snapshots.

Buckets representing fewer than ten wallets are suppressed. Time windows are coarse and fixed to reduce differencing risk. Responses include an ETag and `Cache-Control: public, max-age=300, stale-while-revalidate=60`; breaking field changes require a new API version.

## Timestamps

Every timestamp field is returned twice: the original integer epoch (seconds since the Unix epoch, kept for backward compatibility) and an ISO-8601 companion named `<field>_iso`. The companion is always UTC and ends with a `Z` suffix, so it can be announced directly by screen readers and parsed by clients without extra formatting.

For example, a response containing `"updated_at": 1700000000` also contains `"updated_at_iso": "2023-11-14T22:13:20Z"`. Both values always refer to the same instant; clients may use either, but new integrations should prefer the `_iso` string.

## Display fields

Some response fields are intended to be shown to a user (for example, announced by a screen reader), while others are machine-oriented values such as identifiers, raw counters, or cache metadata. Fields intended for display are marked in `openapi/openapi.yaml` with the `x-display: true` extension. Client authors building accessible interfaces should prefer these fields for human-facing output and treat unmarked fields as machine use only.

The fields intended for display, per endpoint, are:

- `GET /api/v1/public/volume` — `date`, `volume_usd`
- `GET /api/v1/public/open-interest` — `date`, `open_interest_usd`
- `GET /api/v1/public/pnl-distribution` — `bucket`, `wallets`
- `GET /api/v1/public/strategies` — `strategy`, `wallets`
- `GET /api/v1/public/settlements` — `date`, `settled_usd`
- `GET /api/v1/public/insurance-fund` — `date`, `balance_usd`
- `GET /api/v1/public/governance` — `proposal`, `participation_pct`
- `GET /api/v1/public/treasury` — `date`, `balance_usd`
- `GET /api/v1/public/fees` — `date`, `fees_usd`
- `GET /api/v1/public/tvl` — `date`, `tvl_usd`

Every field listed above carries `x-display: true` in the OpenAPI spec. This section does not change any response shape; it only documents which existing fields are meant for display.
