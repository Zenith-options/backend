# Settlement, reconciliation, and collateral

Expiry work is keyed by `(series_id, expiry_ledger)` and leased from durable jobs. Settlement runs as a multi-stage saga rather than a single step, so a failure at any stage leaves the job in a known, recoverable state instead of a half-applied settlement.

## Settlement saga stages

The saga advances through these stages in order:

1. **Pending** — the job is leased from durable jobs and claimed for the `(series_id, expiry_ledger)` key.
2. **Simulation** — the settlement transaction is simulated to validate it before any submission.
3. **Submission** — the simulated transaction is submitted and its hash is recorded.
4. **Confirmation** — the submitted transaction is confirmed on chain.

A submitted job is reconciled by hash and is never blindly resubmitted after restart.

## Failure handling

If a stage fails, the saga does not skip ahead: the job stays at the last durable stage and is retried from there, so a failed simulation never submits and a failed submission is reconciled by hash before any resubmission. Retry policy, backoff, and reconciliation details live in [settlement resilience](settlement-resilience.md) and are not duplicated here.

Reconciliation compares integer-valued snapshots at one ledger and repairs only by replaying the authoritative event range. Vault balances change only after a confirmed vault event; unsigned intents have unique nonces and expire without optimistic accounting writes.

Margin is the maximum portfolio loss across stored scenarios. Maintenance equals that loss and initial margin applies a checked basis-point multiplier, preserving `maintenance <= initial` without floating-point accounting.
