# Merkle-Commitment Batch Settlement

Issue #196. When a pool of option positions expires, the backend commits to
the entire set of payouts with **one Merkle root** carried in **one signed
transaction**, then lets each account claim by proving inclusion. This
document is the normative description of the commitment: an independent
implementation (a Soroban contract verifier, an SDK, an auditor's script) must
be able to reproduce the exact same roots and proofs.

Scope note: the on-chain verifier contract, price/dispute logic, and payout
transfer mechanism are **out of scope** here. This backend produces the
commitment, persists it, and serves proofs and idempotent (partial) claims.

## Data model

Amounts are integers in **stroops** (the asset's smallest unit, 7 decimal
places): `amount_scaled = amount * 10^7`. This matches the backend's
`Money`/`Qty` convention (`MONEY_SCALE = 7`).

A leaf commits to one payout:

| field          | type   | meaning                                        |
| -------------- | ------ | ---------------------------------------------- |
| `account`      | string | payout recipient (`G...` or `C...` address)    |
| `position_id`  | string | the position being settled                     |
| `payout_asset` | string | asset symbol or contract id                    |
| `amount_scaled`| i64    | payout in stroops                              |

## Leaf encoding (v1)

`LEAF_DOMAIN = "ZENITH_SETTLE_LEAF_V1"` (21 ASCII bytes)

```
leaf_hash = SHA-256(
    LEAF_DOMAIN                ||
    u32_le(len(account))       || account_bytes       ||   // UTF-8
    u32_le(len(position_id))   || position_id_bytes   ||   // UTF-8
    u32_le(len(payout_asset))  || payout_asset_bytes  ||   // UTF-8
    i64_le(amount_scaled)                                   // two's complement
)
```

Variable-length fields are **length-prefixed** so the encoding is injective —
`("ab","c")` and `("a","bc")` produce different hashes. `LEAF_VERSION = 1` is
persisted on each batch so a future encoding change can be detected.

## Internal node encoding (v1)

`NODE_DOMAIN = "ZENITH_SETTLE_NODE_V1"` (21 ASCII bytes)

```
node_hash = SHA-256(NODE_DOMAIN || left || right)
```

Leaves and nodes use different domain tags, so a leaf hash can never be
mistaken for a node hash (and vice versa).

## Tree construction

1. **Canonical order.** Leaves are sorted by `(position_id, account)` — a
   total order independent of how the caller supplied them. The sort key must
   be unique; duplicates are rejected at planning time.
2. **Bottom-up pairing.** At each level, adjacent pairs are combined into
   parents. An odd trailing node is **promoted unchanged** to the next level
   (never hashed with itself).
3. **Empty tree.** The root of an empty tree is 32 zero bytes. Empty batches
   are rejected before commitment.

The leaf count is part of the commitment, so promotion cannot be confused
with a differently-shaped tree that happens to share a root.

## Commitment

```
commitment = { root, leaf_count, leaf_version, total_amount_scaled }
```

`total_amount_scaled` is the sum of every leaf's amount. The batch is only
committable when:

* `sum(leaf.amount_scaled) == total_scaled`, **and**
* `total_scaled <= available_scaled` (the operator's collateral).

This is the **conservation check**: the promise cannot exceed the leaves, and
the leaves cannot exceed what is actually held. It is enforced again at
commit time against the persisted numbers.

## Lifecycle

```
PLANNED ──commit──▶ COMMITTED ──finalize──▶ FINALIZED
```

* **PLANNED** — instructions persisted; no root on chain. The commitment is
  computed but not frozen.
* **COMMITTED** — one signed transaction carrying the root has been
  accepted. The leaf set is now **immutable**: the database triggers reject
  any insert/update/delete that would change a leaf. Claims are accepted.
* **FINALIZED** — every leaf fully claimed. Further claims are rejected.

## Claims

* A claim is `(batch_id, position_id, idempotency_key) → amount_scaled`.
* **Idempotent:** replaying the same `idempotency_key` returns the original
  claim with `idempotent_replay: true` and moves no money.
* **Partial:** a leaf may be claimed repeatedly while
  `sum(claims) <= leaf.amount_scaled`. Exceeding it is a `409 over_claim`.
* Only the leaf's own `account` may claim it (`403 not_claimant`).
* When the last outstanding leaf is claimed, the batch auto-finalizes.

## HTTP API

All endpoints require a bearer session token.

| method | path                                                          | purpose                     |
| ------ | ------------------------------------------------------------- | --------------------------- |
| POST   | `/api/v1/settlement/batches`                                  | plan a batch                |
| GET    | `/api/v1/settlement/batches/:id`                              | fetch batch + commitment    |
| POST   | `/api/v1/settlement/batches/:id/commit`                       | commit root (locks leaves)  |
| GET    | `/api/v1/settlement/batches/:id/leaves`                       | list leaves + claimed state |
| GET    | `/api/v1/settlement/batches/:id/leaves/:position_id/proof`    | inclusion proof             |
| POST   | `/api/v1/settlement/batches/:id/claims`                       | claim (full/partial)        |

Stable error codes: `invalid_batch` (422), `leaf_set_locked` (409),
`over_claim` (409), `nothing_to_claim` (409), `invalid_state` (409),
`not_found` (404), `not_claimant` (403).

### Example

```bash
# 1. Plan
curl -s -XPOST localhost:8080/api/v1/settlement/batches \
  -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"payout_asset":"USDC","total_scaled":300000000,
       "instructions":[{"position_id":"p1","account":"G...","amount_scaled":100000000},
                       {"position_id":"p2","account":"G...","amount_scaled":200000000}]}'

# 2. Commit (single signed tx)
curl -s -XPOST localhost:8080/api/v1/settlement/batches/$ID/commit \
  -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"commitment_tx_hash":"..."}'

# 3. Prove
curl -s localhost:8080/api/v1/settlement/batches/$ID/leaves/p1/proof \
  -H "authorization: Bearer $TOKEN"

# 4. Claim (partial, idempotent)
curl -s -XPOST localhost:8080/api/v1/settlement/batches/$ID/claims \
  -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"position_id":"p1","amount_scaled":40000000,"idempotency_key":"k1"}'
```

## Verifying an offline implementation

Given a committed batch's `root` and `leaf_count`, independently:

1. Re-encode each leaf exactly as above and hash it.
2. Sort by `(position_id, account)`.
3. Build the tree with promotions as described.
4. The resulting root must equal the committed `root`, and the leaf count must
   match `leaf_count`.

For a proof: start from the leaf hash and apply each step
(`sibling_is_right` says which side the sibling goes on) using the node
encoding. The final value must equal the committed root.

The Rust core lives in `src/chain/merkle.rs` (encoding/tree/proofs) and
`src/chain/batch.rs` (conservation, claims). Property tests in
`tests/settlement_invariants_test.rs` assert order-independence and proof
validity; `tests/settlement_test.rs` covers the HTTP lifecycle.
