# Settlement resilience and storage lifecycle

Settlement is a write-ahead saga keyed by a unique client idempotency key. The saga runs its stages in order: reserve, price, finalise, and record. Recovery queries any recorded transaction hash before deciding whether to resume; applied compensatable steps unwind in reverse order, while an applied settlement enters a documented non-compensatable terminal state for review. When a stage fails, the saga halts at that stage and either compensates the already-applied steps or defers for retry according to the policy below.

Settlement price is the integer mean of one bounded observation per ledger in the configured close window. Primary observations win; when absent, the secondary bounded feed is used. If neither exists settlement is deferred. A challenge before the deadline blocks finalisation and duplicate challenges are rejected.

Retry behaviour for failed stages is governed by the resilience policy described in this document; the settlement flow in `docs/settlement.md` cross-references it rather than duplicating the rules.

Persistent storage keys are grouped by contract and extended when remaining TTL is below the floor. Day settings are converted using observed ledger close time, targets are clamped to the network maximum, closed keys are inactive, and a bump ledger prevents duplicate work after restart.

The chain invariant test, settlement fuzz targets, and `loadtests/chain.js` cover idempotent replay, deterministic pricing, malformed state transitions, and throttled RPC-facing traffic. They are intentionally bounded for CI and can use larger budgets nightly.
