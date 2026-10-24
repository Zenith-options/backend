# Soroban RPC transport

Chain configuration is optional. Set `SOROBAN_RPC_URL`, `STELLAR_NETWORK_PASSPHRASE`, and optionally `SOROBAN_RPC_TIMEOUT_MS`; without them the paper-trading service remains available and chain health is reported as unavailable.

The transport bounds concurrent requests, applies full-jitter exponential backoff within an overall budget, and opens its shared circuit after a configured consecutive-failure threshold. Only idempotent reads are retryable. `sendTransaction` is intentionally never retried automatically.
