# Soroban RPC transport

Chain configuration is optional. Set `SOROBAN_RPC_URL`, `STELLAR_NETWORK_PASSPHRASE`, and optionally `SOROBAN_RPC_TIMEOUT_MS`; without them the paper-trading service remains available and chain health is reported as unavailable.

The transport bounds concurrent requests, applies full-jitter exponential backoff within an overall budget, and opens its shared circuit after a configured consecutive-failure threshold. Only idempotent reads are retryable. `sendTransaction` is intentionally never retried automatically.

## Retry observability

Each retry attempt emits a single debug-level log line carrying the attempt number and the retry reason (for example `timeout`, `transport_error`, or `http_5xx`). Retries are never logged at info level, so log volume stays bounded even when an upstream is flaky.

Every retry also increments the `rpc_client_retries_total` counter, labelled by `reason`, so a flaky upstream shows up as a rising retry rate rather than unexplained slow requests. The counter is registered at startup alongside the other chain metrics. The retry policy itself (backoff, budget, and circuit thresholds) is unchanged by this instrumentation.
