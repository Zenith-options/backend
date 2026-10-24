# Settlement resilience and storage lifecycle

Settlement is a write-ahead saga keyed by a unique client idempotency key. Recovery queries any recorded transaction hash before deciding whether to resume; applied compensatable steps unwind in reverse order, while an applied settlement enters a documented non-compensatable terminal state for review.

Settlement price is the integer mean of one bounded observation per ledger in the configured close window. Primary observations win; when absent, the secondary bounded feed is used. If neither exists settlement is deferred. A challenge before the deadline blocks finalisation and duplicate challenges are rejected.

Persistent storage keys are grouped by contract and extended when remaining TTL is below the floor. Day settings are converted using observed ledger close time, targets are clamped to the network maximum, closed keys are inactive, and a bump ledger prevents duplicate work after restart.

The chain invariant test, settlement fuzz targets, and `loadtests/chain.js` cover idempotent replay, deterministic pricing, malformed state transitions, and throttled RPC-facing traffic. They are intentionally bounded for CI and can use larger budgets nightly.
