# Settlement statements and confirmations

`GET /api/v1/settlement/statements?from=<unix>&to=<unix>&format=csv|pdf`
renders the authenticated wallet's settlement activity in stable
`(occurred_at, id)` order. Supply `record_id` to render a single activity
record as a trade confirmation. The endpoint rejects ranges longer than 366
days and periods containing more than 50 records. Empty periods produce a
header-only CSV or a PDF that says there was no activity.

Statements are rendered from `settlement_activity`, which stores exercises,
assignments, transfers, cash movements, and fees with their chain transaction
hashes. The schema and both renderers avoid request-time timestamps, so the
same stored rows and query produce byte-identical output.

The current repository has no configured document-signing key or chain-event
projection writer for this SQLite service. This endpoint therefore emits
deterministic, unsigned reports; it must not be represented as a signed
authoritative statement until those integrations are provided.
