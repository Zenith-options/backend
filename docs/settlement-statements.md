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

## Download metadata

Statement responses are delivered as downloads so keyboard and screen-reader
users can identify the file before activating it:

- `Content-Disposition: attachment; filename="statement-<period>.<ext>"`,
  where `<period>` is the requested range (for example `2024-01-01_2024-01-31`)
  or the confirmation's `record_id`, and `<ext>` is `csv` or `pdf`.
- `Content-Type` matches the emitted format: `text/csv; charset=utf-8` for CSV
  and `application/pdf` for PDF.

Any listing endpoint that exposes statements includes a human-readable
`label` field describing the statement period (for example
`2024-01-01 to 2024-01-31`) so each entry has accessible link text.

The current repository has no configured document-signing key or chain-event
projection writer for this SQLite service. This endpoint therefore emits
deterministic, unsigned reports; it must not be represented as a signed
authoritative statement until those integrations are provided.
