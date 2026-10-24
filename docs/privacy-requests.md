# Settlement data export and erasure

Authenticated users can request a JSON export from `GET /api/v1/privacy/export`.
The current bundle contains the account summary and the wallet's complete
position lifecycle history. `DELETE /api/v1/privacy/erasure` removes sessions,
nonces, API keys, alerts, watchlists, delivery events, delivery channels, and
webhook endpoints in one transaction.

Erasure is rejected with `409 Conflict` while the wallet has an open position.
The accounts and positions tables are retained as financial records, and the
public wallet address is kept as a pseudonymous chain/audit anchor. The request
is recorded in `privacy_erasure_log`. This service does not delete or alter
on-chain data.

The repository's settlement/indexer projections use a separate PostgreSQL
schema; this SQLite application currently has no per-wallet projection reader.
Those chain-side projections are therefore not part of this export bundle.
