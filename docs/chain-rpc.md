# Soroban RPC transport

Chain configuration is optional. Set `SOROBAN_RPC_URL`, `STELLAR_NETWORK_PASSPHRASE`, and optionally `SOROBAN_RPC_TIMEOUT_MS`; without them the paper-trading service remains available and chain health is reported as unavailable.

The transport bounds concurrent requests, applies full-jitter exponential backoff within an overall budget, and opens its shared circuit after a configured consecutive-failure threshold. Only idempotent reads are retryable. `sendTransaction` is intentionally never retried automatically.

## Retry policy

The retry count, backoff, and retried error classes below are the values the RPC client actually uses; tune timeouts against these, not against any older figures.

- **Retry count:** a retryable read is attempted up to the client's configured maximum attempts (the retry constant in the client code), not an unbounded loop. Once the attempt budget is exhausted the last error is surfaced.
- **Backoff:** full-jitter exponential backoff between attempts, capped by the overall request budget. The delay is randomized across the exponential window rather than a fixed sleep, so wall-clock latency per retry varies.
- **Retried errors:** only transient transport/availability failures on idempotent reads are retried (e.g. timeouts, connection failures, and rate-limit/5xx-style responses). Non-idempotent calls such as `sendTransaction` are never retried.
- **Terminal errors:** validation/decoding errors and other non-transient failures are not retried and are returned immediately.

> Note: if the client's match arms retry an error class that earlier documentation described as terminal, that class is retried in practice — the code's match arms are authoritative. Confirm the exact retry count and backoff constants against the client source before tuning timeouts.
