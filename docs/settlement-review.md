# Settlement batch review

Settlement batches move collateral unattended. `src/settlement_review.rs`
puts an exact preview and, for high-value batches, a human between "the
keeper decided to settle" and "funds moved".

## Lifecycle

```
                 ┌─ within policy ─▶ auto_approved ─┐
propose ─▶ preview                                  ├─▶ execute ─▶ executing ─▶ executed
                 └─ needs review ─▶ pending_review ─┤                    └─▶ failed ─(retry)─┘
                                       │   approve ─┘
                                       └─ reject ─▶ rejected

Any not-yet-executed batch ─▶ stale            (live data no longer matches the preview,
                                                or a newer proposal superseded it)
                           ─▶ expired          (approval window elapsed)
                           ─▶ compliance_hold  (a party failed screening)
```

`executed`, `rejected`, `stale`, `expired` and `compliance_hold` are
terminal. A database trigger rejects any status change out of them. Only
one live batch may exist per `(underlying, expires_at)`.

## The preview

`build_preview(pool, underlying, expires_at, fixing_price, fixing_source)`
returns every number the batch will move, in integer stroops:

* **Per leg:** payoff, collateral unlocked, the part of it paid out,
  shortfall, and the change to the holder's free balance.
* **Per wallet:** net cash change, collateral unlocked, shortfall.
* **Totals:** long payout, short paid, collateral unlocked, shortfall,
  `platform_net` (what the platform counterparty pays), `gross_value`
  (long payout plus collateral unlocked), and the largest single-wallet
  credit.
* **`sha256`:** the hash of all of the above, including the fixing price
  and fixing source. Leg order does not affect it.

The preview is computed by the same code at proposal time and again
immediately before execution. If the two hashes differ (a position was
opened, closed or edited after review), the batch is marked `stale` and
nothing moves. **What executes is what was reviewed.**

## Policy (`ApprovalPolicy`)

A batch needs human review if any of these is true:

| Rule | Env var | Default |
|---|---|---|
| `gross_value` over the limit | `ZENITH_SETTLEMENT_AUTO_APPROVE_MAX` (units) | 10,000 |
| Any wallet credited over the limit | `ZENITH_SETTLEMENT_AUTO_APPROVE_MAX_WALLET` (units) | 2,500 |
| Any collateral shortfall | — | on |
| Fixing source is `fallback` or `manual` | — | on |
| Review everything | `ZENITH_SETTLEMENT_ALWAYS_REVIEW` | `false` |

The reasons are stored with the batch (`review_reasons`) and shown to the
reviewer. Approvals and auto-approvals stay executable for
`ZENITH_SETTLEMENT_APPROVAL_TTL_SECS` (default 3600). After that the batch
is `expired` and must be re-proposed. Consider `ALWAYS_REVIEW=true` for
the first weeks on mainnet.

## Approving

```rust
approve(&pool, batch_id, "alice@zenith", &reviewed_sha256, "fixing checked against oracle", &policy, now)
```

* The approver must not be the proposer (four-eyes).
* `reviewed_sha256` must be the hash the reviewer was shown, so an
  approval cannot be applied to a preview nobody looked at.
* `reject` requires a note.

Review checklist:

1. Is `fixing_price` consistent with the oracle and the venue TWAP? For a
   `fallback` fixing, why were ticks missing?
2. Does `platform_net` make sense for the open interest?
3. Is every `shortfall` explained? A shortfall means collateral was
   insufficient, which should not happen with correct margining.
4. Are the largest wallet credits plausible?

## Execution

`execute(&ctx, batch_id, actor, now)`, in order:

1. Refuses unless the batch is `approved`, `auto_approved` or `failed`.
2. Expired approval → `expired`.
3. Recomputes the preview; hash mismatch → `stale`.
4. `PinGuard::ensure_live` on the settlement contract. A contract that
   is unpinned or was changed blocks execution, and the batch stays
   executable for when it is fixed.
5. Screens every wallet in the batch (`Flow::Settlement`). Any enforced
   denial → `compliance_hold` for the **whole** batch. Settling against a
   blocked party's collateral is itself a dealing in blocked property, so
   the batch is not partly settled.
6. Claims the batch (`executing`) with a conditional update, so two
   executors cannot both run it.
7. Calls `SettlementExecutor::execute`. Success → `executed` with the
   transaction reference. Error → `failed`, which can be retried.
   **Executors must be idempotent on `batch_id`**, so a retry cannot
   settle twice.

## Audit

`settlement_batch_events` is append-only (triggers reject UPDATE and
DELETE) and records every transition with actor and detail. The preview
columns of `settlement_batches` are immutable, and batches cannot be
deleted.

```sql
-- The review queue.
SELECT id, underlying, expires_at, gross_value, review_reasons, proposed_at
FROM settlement_batches WHERE status = 'pending_review' ORDER BY proposed_at;

-- Who did what to a batch.
SELECT created_at, event, actor, detail FROM settlement_batch_events
WHERE batch_id = ? ORDER BY id;
```
