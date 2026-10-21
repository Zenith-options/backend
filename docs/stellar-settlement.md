# Stellar wallet signing, Soroban authorization, and RFQ settlement

This backend does not yet contain deployed vault/oracle/settlement contracts,
Stellar SDK transaction construction, or a production Soroban submission
configuration. Its current transaction builder and submitter are placeholders.
The boundaries below are therefore explicit: helper types and RFQ records do
not claim to produce a valid contract transaction or move funds on-chain.

## SEP-7 wallet handoff

`chain::sep7::transaction_signing_uri` formats a SEP-7
`web+stellar:tx` URI for a base64-encoded envelope and HTTPS callback. The
transaction must first be built and simulated by a trusted server-side
transaction builder. Do not pass arbitrary client-supplied XDR into a signing
URI and treat a wallet's signature as authorization for a settlement.

The production flow still needs to:

1. Build the specific deposit, withdrawal, exercise, or close-out invocation
   from authenticated, server-validated settlement intent.
2. Simulate against the selected network and show the wallet the complete
   operation and fee summary.
3. Persist a short-lived, single-use callback state bound to the wallet,
   network, action, and unsigned transaction.
4. Return the SEP-7 URI; on callback, decode and verify signed XDR with the
   Stellar SDK, check the original operation tree/network/expiry and wallet
   signature, consume callback state atomically, then submit through the
   configured Soroban RPC and record the returned hash.
5. Poll transaction status and report final contract success or failure.

SEP-7 transaction-envelope signing and Soroban address authorization are
distinct signatures. The server must never substitute its own signature for
the user's wallet.

## Soroban address authorization boundary

The `chain::authorization` types model the invocation tree returned by
simulation. They are deliberately not serialized as Soroban XDR and do not
create signatures. A future SDK-backed assembler must use the exact auth entry
and invocation tree from simulation; it must not widen that tree by inserting
unrequested sub-invocations.

For the intended contract split:

- The user authorizes the settlement entry point for the exact action, amount,
  position/series, and recipient.
- The settlement contract reads the oracle through a read-only call. A price
  read must not request user authorization or grant oracle access to move
  collateral.
- The settlement contract invokes the vault using contract authorization
  (`authorize_as_current_contract`) or the vault's explicitly defined
  settlement-role check. The vault must independently restrict which
  settlement contract can move which assets.
- A user authorization for one root invocation only covers the declared
  invocation subtree. A call requiring a different address's authorization
  needs its own address-auth entry; it does not inherit the user's authority.
- Backend authorization payload assembly must use the network passphrase,
  nonce, expiration ledger, exact invocation XDR, and the Soroban SDK's
  authorization preimage rules. Simulation output must not be treated as a
  verified signature.

Contract IDs, function signatures, argument XDR, role initialization, and
upgrade/admin policy must be fixed from the deployed contracts before the
backend can safely generate or validate these entries.

## RFQ API currently available

All endpoints require a live bearer session. RFQs are visible only to the
requester and the maker wallets explicitly invited in `counterparties`.
Amounts use integer micro-USDC and quote IDs are single-RFQ scoped.

### `POST /api/v1/rfqs`

```json
{
  "series": "BTC-2026-12-18-C-100000",
  "side": "buy",
  "contracts": 100,
  "counterparties": ["G..."],
  "ttl_secs": 300
}
```

The side is `buy` or `sell`; the contract count must be positive; the TTL is
30–3600 seconds. Counterparties must be unique Stellar public-key addresses.

### `POST /api/v1/rfqs/{id}/quotes`

An invited maker posts `premium_microusd` and an optional `ttl_secs`. A maker
may update its offer while the RFQ is open. Offer expiry cannot outlive the
RFQ. Makers see only their own quote; the requester sees all invited offers.

### `POST /api/v1/rfqs/{id}/accept`

The requester submits `{ "quote_id": "..." }`. A guarded database transaction
accepts one live quote and withdraws competing offers. This guarantees
single-winner negotiation state, not delivery-versus-payment.

### `GET /api/v1/rfqs/{id}` and `POST /api/v1/rfqs/{id}/cancel`

Read is restricted to the requester or an invited maker. Only the requester
can cancel an open RFQ.

## Atomic block delivery-versus-payment still required

The RFQ endpoints currently negotiate and record an off-chain agreement only.
They do not create positions, move collateral/premium, or submit an on-chain
transaction. A deployed settlement contract must expose a single block-settle
entry point that validates the accepted RFQ/quote terms and atomically performs
both asset legs and the position ownership change. Its transaction must fail
as a whole if any leg fails; the backend should only mark settlement complete
after the Soroban transaction is confirmed successful.

Do not present an accepted RFQ as a settled trade. API responses identify this
explicitly with `settlement: "off_chain_agreement_only"`.
