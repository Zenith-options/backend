# Zenith Backend

Rust/Axum API for Zenith, a decentralized options protocol on Stellar
Soroban. Black-Scholes pricing with a crypto vol smile, a paper-trading
account/positions ledger backed by SQLite, sign-in-with-wallet auth, and
a live spot-price WebSocket feed.

## Status

Market data (spot prices, vol surface) is in-memory and nudged by a
background simulator — there's no real price feed or on-chain
integration yet. Everything else (accounts, positions, watchlist,
alerts) persists to a SQLite file via sqlx. This is a paper-trading
backend for the frontend to build against, not a production trading
system.

## Getting started

```bash
cp .env.example .env   # DATABASE_URL=sqlite://zenith.db, or leave unset for the same default
cargo run
# listening on 0.0.0.0:8081
```

```bash
cargo test              # 11 unit tests + 29 integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

No external services required — sqlx creates and migrates the SQLite
file on first run, and every integration test spins up its own
throwaway temp-file database.

## Endpoints

All `/api/v1/*` endpoints marked **auth** require an
`Authorization: Bearer <token>` header from `/api/v1/auth/verify`.

### Market data (public)

| Endpoint | What it does |
|---|---|
| `GET /health` | Liveness + a DB ping |
| `GET /api/v1/spot` | Current spot prices + base vols for all underlyings |
| `GET /api/v1/price` | Black-Scholes premium/Greeks for one option |
| `GET /api/v1/iv` | Implied vol for a given market price (Newton-Raphson) |
| `GET /api/v1/chain` | Full option chain (calls+puts) across strikes for one expiry |
| `GET /api/v1/expiries/:underlying` | Available expiries for an underlying |
| `GET /api/v1/stats` | Protocol-wide stats (mocked, not derived from real trades) |
| `GET /api/v1/ws/spot` | WebSocket: snapshot on connect, then a live tick every ~2s |
| `POST /api/v1/portfolio/payoff` | Combined P&L curve for a set of caller-supplied legs (no auth — legs carry their own premium) |

### Auth (public, rate-limited)

| Endpoint | What it does |
|---|---|
| `POST /api/v1/auth/nonce` | Issue a single-use, 5-minute sign-in message for a wallet address |
| `POST /api/v1/auth/verify` | Verify the signed message, get a 24h bearer session token |
| `GET /api/v1/auth/me` **auth** | Confirm the current token's wallet address |

Requests are rate-limited with Redis-backed GCRA when `REDIS_URL` is set;
Redis failures fall back to a per-process GCRA limiter. Anonymous IPs,
authenticated wallets, and callers presenting `X-API-Key` receive
separate tiers (120, 300, and 1,200 credits/minute, respectively).
Weighted operations consume more credits: option chains cost 5, payoff
curves cost 4, pricing/IV queries cost 2, and POST requests cost 2 by
default. Responses include `RateLimit-Limit`, `RateLimit-Remaining`, and
`RateLimit-Reset` headers, plus `Retry-After` on throttling. API-key
issuance and authorization are not implemented by this service; the
header currently identifies the rate-limit tier and principal only.

### Account & positions **auth**

| Endpoint | What it does |
|---|---|
| `GET /api/v1/account` | Balance + locked collateral |
| `GET /api/v1/positions` | List positions (`?status=`, `?strategy_id=`, `?limit=`, `?offset=`) |
| `POST /api/v1/positions/open` | Price and open one position |
| `POST /api/v1/positions/:id/close` | Settle an open position at current spot/vol |
| `POST /api/v1/positions/:id/roll` | Close + reopen at a new strike/expiry, atomically |
| `GET /api/v1/history` | Closed/rolled positions + win/loss/pnl stats (`?limit=`, `?offset=` — stats always cover the full history, not just the returned page) |
| `GET /api/v1/portfolio/greeks` | Aggregate Greeks across all open positions, repriced live |
| `POST /api/v1/strategies/execute` | Open 2+ legs atomically under one shared `strategy_id` |
| `GET /api/v1/strategies` | List multi-leg strategies with aggregate realized/unrealized P&L |
| `GET /api/v1/strategies/:id` | One strategy's legs + aggregate P&L |
| `POST /api/v1/strategies/:id/close` | Close every currently-open leg of a strategy atomically |

### Watchlist & alerts **auth**

| Endpoint | What it does |
|---|---|
| `GET` / `POST /api/v1/watchlist` | List / add a watched symbol |
| `DELETE /api/v1/watchlist/:underlying` | Remove a watched symbol |
| `GET` / `POST /api/v1/alerts` | List / create a price alert (`above`/`below` a target) |
| `DELETE /api/v1/alerts/:id` | Remove an alert |

Alerts are checked against spot every 10s by a background task; a
triggered alert stays in the table (visible via GET) rather than being
deleted.

Every response carries an `x-request-id` header — a fresh UUIDv4 if the
request didn't already have one, or the caller's own value echoed back
unchanged otherwise — for tracing a single request through logs.

### Supply-chain checks and releases

Pull requests run `cargo deny check` for advisories, licenses, duplicate
crates, and registry/source policy. A weekly scheduled `cargo audit`
files or comments on an issue when RustSec advisories are found.
Publishing a GitHub Release generates a CycloneDX JSON SBOM and attaches
`sbom.json` to that release. Dependabot checks Cargo and GitHub Actions
dependencies weekly.

### Kubernetes

The Helm chart is in `deploy/helm/zenith-backend`. Install staging or
production with:

```bash
helm upgrade --install zenith deploy/helm/zenith-backend \
  --namespace zenith --create-namespace \
  -f deploy/helm/zenith-backend/values-production.yaml
```

The External Secrets Operator and a Prometheus Adapter exposing
`http_requests_per_second` must be installed in the cluster. The External
Secret must provide `DATABASE_URL` and `REDIS_URL`; set the database URL
to `sqlite:///data/zenith.db` when using the chart's shared volume. The
chart requests a ReadWriteMany PVC for the API and worker. This preserves
SQLite data across pods but does **not** make SQLite a safe multi-writer
database: use a single API replica or migrate to a server database before
using horizontal scaling for production traffic.

## Architecture

```
src/
├── main.rs          # Thin entrypoint: init_tracing -> init_state -> build_router -> serve
├── lib.rs           # Pricing engine, AppState, request/response types, route wiring
├── db.rs            # SQLite pool + migration runner
├── models.rs        # Row structs (Account, Position, WatchlistItem, Alert)
├── error.rs         # AppError: JSON {"error": "..."} instead of empty-body status codes
├── auth.rs          # Sign-in-with-wallet: nonce, verify, AuthUser extractor, session cleanup
├── strkey.rs         # Stellar G... address <-> raw ed25519 pubkey codec
├── collateral.rs    # Collateral rules for writing options (100% calls, 110% puts)
├── payoff.rs         # Combined multi-leg P&L math (ported from the frontend's lib/payoff.ts)
├── positions.rs      # Account/position/roll/greeks handlers + the open/close tx helpers
├── strategies.rs     # Multi-leg atomic execution, built on positions.rs's tx helpers
├── history.rs         # Closed/rolled positions + stats
├── request_id.rs      # UUIDv4 generator for the x-request-id middleware
├── watchlist.rs, alerts.rs, prices.rs  # Per-domain CRUD + background loops
migrations/           # One file per schema change, embedded into the binary at compile time
tests/
├── common/mod.rs     # TestApp: real router over a throwaway temp-file DB, via tower::oneshot
└── *_test.rs         # One file per domain, black-box HTTP-level assertions
```

`main.rs` is intentionally thin — everything testable lives in the
`zenith_backend` library crate, which is what lets `tests/*_test.rs`
exercise the real router without a bin-only crate's usual restriction
(a `tests/` directory can only see a *library* crate's public items).

### A note on the pricing model

`smile_vol()` is a bit-for-bit port of the frontend's `smileVol()` —
including its wing term being unconditionally `(|moneyness-1|-0.15)^2`
rather than clamped at zero near the money, which looks like a bug but
matches the frontend's shipped (if quirky) behavior on purpose, for
pricing parity between client and server.

### A note on time-to-expiry

Closing/rolling a position reprices it with the *same* time-to-expiry
it was opened with, rather than tracking an absolute expiry timestamp
and computing real elapsed time. Fine for a paper-trading demo; not a
real theta-decay model.

## Known gaps

- No real market data feed — spot prices are seeded constants nudged by
  a random-walk simulator, not sourced from anywhere real.
- No on-chain / Soroban integration — this is pure off-chain paper
  trading.
- `Dockerfile` and the CI workflow are not build/run-tested against a
  real Docker daemon or GitHub Actions runner from this environment —
  reviewed for correctness, not executed end-to-end.

Previously listed here and since addressed: the three background loops
(auth cleanup, alert checks, price simulator) now have direct unit
tests against their extracted per-tick logic; the rate limiter moved
`list_positions`/`get_history` now report
total count and whether more pages exist (`x-total-count`/`x-has-more`
headers on positions, a `has_more` field on history) instead of
leaving a paging client to guess; malformed query params and JSON
bodies now return the same `{"error": "..."}` shape as every other
failure instead of axum's plain-text rejections (`AppQuery`/`AppJson`
in `error.rs`); every remaining unexpected-DB-failure path across the
whole app now names the specific operation that failed (`db_error()`
in `error.rs`) instead of a bare "Internal Server Error" with no other
detail; and cross-wallet ownership on close/roll (a stranger 404s
trying to close or roll another wallet's position, same as it already
did for deleting someone else's alert/watchlist entry) and
roll_position's replacement-leg validation are now actually tested
rather than just assumed from reading the SQL.

## License

MIT © Zenith Protocol Contributors
