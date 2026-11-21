# Settlement API and SDK

Authenticated `/api/v1/chain/*` reads return `{ data, as_of_ledger, stale }`. Account routes compare the bearer-session wallet with the requested wallet. Transaction history uses durable cursors; wallet event subscriptions reconnect with the last observed cursor and may drop non-critical updates for slow consumers without blocking indexing.

The TypeScript `SettlementClient` exposes settlement, transaction, vault, margin, liquidation, and WebSocket methods. Signing handoffs contain unsigned XDR, network passphrase, validity ledger, and required public signers only. Private keys and signed envelopes are never logged or retained by the SDK.

## Chain status

`GET /api/v1/chain/status` returns the chain status response. The serialized field names are `ledger` and `timestamp`, matching the Rust response struct in `src/api/` and `openapi/openapi.yaml`:

```json
{
  "ledger": 1234567,
  "timestamp": "2024-01-01T00:00:00Z"
}
```

Clients should read `ledger` (latest ledger number) and `timestamp` (time the status was observed). The earlier `latest_ledger` and `synced_at` names are not emitted and must not be used.
