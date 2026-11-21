# Error Catalogue

This document lists the error codes returned by the API and how they map to HTTP statuses, contract codes, and RPC codes. The canonical source of truth is [`docs/error-codes.json`](./error-codes.json); this catalogue is generated from it.

## Adding a new error code

1. Add the code to the source (the API/contract/RPC layer that emits it).
2. Add a matching entry to [`docs/error-codes.json`](./error-codes.json) with its `code`, `http_status`, `description`, and any `contract_code`/`rpc_code`.
3. Regenerate this catalogue from the JSON:
   ```bash
   node scripts/generate-error-catalogue.mjs
   ```
4. Commit both `docs/error-codes.json` and the regenerated `docs/error-catalogue.md` together.

Do not hand-edit the generated sections of this file; edit `error-codes.json` and re-run the generator instead.

## Codes

<!-- The table below is generated from docs/error-codes.json. -->

| Code | HTTP status | Description |
| --- | --- | --- |
| `CLIENT_ERROR` | 400 | Unclassified client-side failure. |
| `CONFLICT` | 409 | The request conflicts with current resource state. |
| `FORBIDDEN` | 403 | The authenticated caller is not permitted to perform this operation. |
| `INSUFFICIENT_BALANCE` | 422 | The account balance or available collateral is insufficient. |
| `INTERNAL_ERROR` | 500 | Unexpected server-side failure. |
| `INVALID_CREDENTIALS` | 400, 401 | A supplied wallet address or signature credential is invalid. |
| `INVALID_REQUEST` | 400 | The request is malformed or has invalid fields. |
| `NOT_FOUND` | 404 | The requested resource does not exist or is not visible to this caller. |
| `RATE_LIMITED` | 429 | The caller exceeded the configured request rate. |
| `SERVICE_UNAVAILABLE` | 503 | A required dependency is unavailable. |
| `UNAUTHENTICATED` | 401 | Authentication is missing, invalid, expired, or failed. |
| `UNPROCESSABLE_ENTITY` | 422 | The request is syntactically valid but cannot be processed. |
| `INSUFFICIENT_COLLATERAL` | 422 | Insufficient collateral for the requested action. |
| `SERIES_EXPIRED` | 409 | The referenced series has expired. |
| `SERIES_NOT_FOUND` | 404 | The referenced series does not exist. |
| `MISSING_TRUSTLINE` | 422 | A required trustline is missing. |
| `UNAUTHORIZED` | 403 | The caller is not authorized for this action. |
| `BUDGET_EXCEEDED` | 422 | The transaction would exceed the network CPU instruction budget. |
| `STORAGE_ARCHIVED` | 422 | A storage entry required by the transaction has been archived. |
| `AUTH_FAILURE` | 403 | Soroban authentication requirements were not satisfied. |
| `RPC_METHOD_UNSUPPORTED` | 503 | The Soroban RPC endpoint does not support the requested method. |
| `RPC_UNAVAILABLE` | 503 | The Soroban RPC endpoint could not complete the request. |
| `UNKNOWN` | 500 | An unrecognized contract, host, or RPC failure occurred. |
