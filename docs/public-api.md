# Public transparency API

`/api/v1/public/*` serves only precomputed protocol aggregates for daily volume, open interest, P&L distributions, strategy popularity, settlements, insurance fund history, governance participation, treasury, fees, and DefiLlama-compatible TVL/volume snapshots.

Buckets representing fewer than ten wallets are suppressed. Time windows are coarse and fixed to reduce differencing risk. Responses include an ETag and `Cache-Control: public, max-age=300, stale-while-revalidate=60`; breaking field changes require a new API version.

## Pagination links

Paginated list responses include a `links` object alongside the cursor fields. Each entry carries an `href` and a `rel` so assistive technology can announce "next page" versus "previous page". `rel` is exactly `next` or `prev`.

```json
{
  "data": [],
  "next_cursor": "c2V0dGxlbWVudHM6MjAyNC0wNi0wMQ==",
  "prev_cursor": "c2V0dGxlbWVudHM6MjAyNC0wNS0wMQ==",
  "links": {
    "next": { "href": "/api/v1/public/settlements?cursor=c2V0dGxlbWVudHM6MjAyNC0wNi0wMQ==", "rel": "next" },
    "prev": { "href": "/api/v1/public/settlements?cursor=c2V0dGxlbWVudHM6MjAyNC0wNS0wMQ==", "rel": "prev" }
  }
}
```

A link is omitted entirely when there is no such page rather than emitted as `null`. On the first page only `next` is present; on the last page only `prev` is present.
