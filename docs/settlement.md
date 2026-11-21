# Settlement, reconciliation, and collateral

Expiry work is keyed by `(series_id, expiry_ledger)` and leased from durable jobs. State advances through pending, simulation, submission, and confirmation; a submitted job is reconciled by hash and is never blindly resubmitted after restart.

Reconciliation compares integer-valued snapshots at one ledger and repairs only by replaying the authoritative event range. Vault balances change only after a confirmed vault event; unsigned intents have unique nonces and expire without optimistic accounting writes.

Margin is the maximum portfolio loss across stored scenarios. Maintenance equals that loss and initial margin applies a checked basis-point multiplier, preserving `maintenance <= initial` without floating-point accounting.

## CLI status output

The settlement CLI never relies on colour alone to convey status. Every status line begins with a bracketed word so it remains readable on monochrome terminals and for users who cannot distinguish colours:

- `[SETTLED]` — the order settled successfully.
- `[FAILED]` — the order failed.
- `[PENDING]` — the order is still in flight.

Colour is an additional cue only. It is disabled when the `NO_COLOR` environment variable is set (to any value) and when stdout is not a TTY, so non-interactive or monochrome terminals receive plain text with no escape codes.
