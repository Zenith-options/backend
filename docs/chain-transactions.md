# Chain transaction lifecycle

Every invocation follows one path: allocate a source sequence, simulate, reject archived footprints or fees above the ceiling, assemble the authoritative footprint/auth entries, sign through an opaque signer, submit once, then persist and poll by hash. Ambiguous submission failures must query the hash before any operator retry.

Prepared envelopes expire five ledgers after simulation. Fee bumps wrap an already signed inner envelope and enforce the same configured ceiling. Confirmation tracking uses bounded exponential polling and records hashes outside RPC retention as `expired_unknown` rather than polling forever.
