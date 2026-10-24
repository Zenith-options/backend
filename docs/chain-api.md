# Settlement API and SDK

Authenticated `/api/v1/chain/*` reads return `{ data, as_of_ledger, stale }`. Account routes compare the bearer-session wallet with the requested wallet. Transaction history uses durable cursors; wallet event subscriptions reconnect with the last observed cursor and may drop non-critical updates for slow consumers without blocking indexing.

The TypeScript `SettlementClient` exposes settlement, transaction, vault, margin, liquidation, and WebSocket methods. Signing handoffs contain unsigned XDR, network passphrase, validity ledger, and required public signers only. Private keys and signed envelopes are never logged or retained by the SDK.
