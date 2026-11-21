# Chain transaction lifecycle

Every invocation follows one path: allocate a source sequence, simulate, reject archived footprints or fees above the ceiling, assemble the authoritative footprint/auth entries, sign through an opaque signer, submit once, then persist and poll by hash. Ambiguous submission failures must query the hash before any operator retry.

Prepared envelopes expire five ledgers after simulation. Fee bumps wrap an already signed inner envelope and enforce the same configured ceiling. Confirmation tracking uses bounded exponential polling and records hashes outside RPC retention as `expired_unknown` rather than polling forever.

## Transaction status

The transaction state enum under `src/` defines the following variants. Every status reported by the API is one of these values.

| Status | Meaning |
| --- | --- |
| `pending` | Submitted and awaiting confirmation. |
| `success` | Confirmed on-chain and applied successfully. |
| `failed` | Confirmed on-chain but rejected by the contract. |
| `expired_unknown` | Hash fell outside RPC retention before confirmation could be observed. |
