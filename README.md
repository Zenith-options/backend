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

Runtime settings are loaded in precedence order: built-in defaults, an
optional TOML file (`--config path/to/config.toml`), environment variables,
then CLI flags. Use `ZENITH_`-prefixed variables (for example
`ZENITH_BIND_ADDRESS=127.0.0.1:9000`) or the legacy `DATABASE_URL`. CLI flags
use kebab-case equivalents such as `--bind-address 127.0.0.1:9000` and
`--session-ttl-secs 3600`. Seed maps can be set in TOML or as JSON through
`ZENITH_SEEDED_PRICES` / `ZENITH_SEEDED_VOLS`. Startup validation rejects
invalid addresses, non-SQLite URLs, nonpositive TTL/rate limits, and invalid
market seeds. Database URLs are redacted from the configuration's debug view.

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

The same routes are available under `/api/v2`; v2 shares handlers with v1
where the contract is unchanged. `GET /api/v2/spot` uses the v2 DTO
(`assets.{symbol}.{price,implied_vol}`), while v1 retains its `prices` and
`vols` maps. `GET /api/versions/usage` reports request totals for each API
version.

Deprecation headers are opt-in and are only added to exact v1 paths listed
in `deprecated_routes`. Configure `deprecation_timestamp` as a Unix timestamp
and `sunset_date` as an RFC 1123 HTTP-date; deprecated routes require both.
Equivalent environment variables are `ZENITH_DEPRECATED_ROUTES` (JSON array),
`ZENITH_DEPRECATION_TIMESTAMP`, and `ZENITH_SUNSET_DATE`. No routes are
deprecated by default.

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

Every `POST` below (positions open/close/roll, watchlist/alerts create,
strategies/execute) is rate-limited per-IP (5/s, burst 20) on top of
requiring a session — see `mutation_rate_limited_routes()` in `lib.rs`.

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

## Client SDKs

The OpenAPI contract in `openapi/openapi.yaml` generates TypeScript and
Rust clients under `sdk/`. Regenerate them with Docker and Node.js:

```bash
node scripts/generate-sdks.mjs 0.1.0
```

The TypeScript package also exports `signInWithWallet()` and
`subscribeToSpot()` helpers from `sdk/typescript/src/zenith.ts`. CI
regenerates both clients and checks that generated files are current.
Published GitHub releases publish the TypeScript package to npm and the
Rust crate to crates.io; configure the repository secrets `NPM_TOKEN`
and `CARGO_REGISTRY_TOKEN` to enable publishing.

### Error responses

V2 errors use `{"error":{"code":"INSUFFICIENT_BALANCE","message":"...","details":{},"request_id":"..."}}`.
Codes are stable machine identifiers listed in `docs/error-catalogue.md`;
`docs/error-codes.json` is the source catalogue. V1 preserves the original
`{"error":"message"}` body by default for existing clients. V1 clients can
request the structured shape with `X-API-Error-Format: structured`, and any
version can request the legacy body with `X-API-Error-Format: legacy`.
`request_id` matches the `x-request-id` response header.

## Architecture

```
src/
├── main.rs          # Thin entrypoint: init_tracing -> init_state -> build_router -> serve
├── lib.rs           # Pricing engine, AppState, request/response types, route wiring
├── db.rs            # SQLite pool + migration runner
├── models.rs        # Row structs (Account, Position, WatchlistItem, Alert)
├── error.rs         # Stable coded error envelopes, request IDs, and v1 compatibility
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
- The mutation rate limiter's bearer-token fallback (for requests with
  no token at all) keys on peer IP only, without replicating
  SmartIpKeyExtractor's x-forwarded-for/x-real-ip/forwarded header
  chain — acceptable since that fallback path is only reached by
  requests that fail AuthUser's own check regardless, but it does mean
  that one specific path isn't proxy-aware the way the auth endpoints'
  limiter is.

Previously listed here and since addressed: the three background loops
(auth cleanup, alert checks, price simulator) now have direct unit
tests against their extracted per-tick logic; the rate limiter moved
off a single global quota to per-IP (`SmartIpKeyExtractor`) on the auth
endpoints and per-wallet (`BearerOrIpKeyExtractor`) on every mutating
one (positions, watchlist, alerts, strategies), so wallets sharing an
IP no longer share a quota; `list_positions`/`get_history` now report
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
