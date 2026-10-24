# Settlement, reconciliation, and collateral

Expiry work is keyed by `(series_id, expiry_ledger)` and leased from durable jobs. State advances through pending, simulation, submission, and confirmation; a submitted job is reconciled by hash and is never blindly resubmitted after restart.

Reconciliation compares integer-valued snapshots at one ledger and repairs only by replaying the authoritative event range. Vault balances change only after a confirmed vault event; unsigned intents have unique nonces and expire without optimistic accounting writes.

Margin is the maximum portfolio loss across stored scenarios. Maintenance equals that loss and initial margin applies a checked basis-point multiplier, preserving `maintenance <= initial` without floating-point accounting.
