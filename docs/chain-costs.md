# Chain cost attribution and budget guardrails

Issue #230. Code: `src/chain_cost/`. Schema:
`migrations/0013_chain_cost_attribution.sql`.

Every Soroban operation costs real XLM. This module answers "what did that
settlement batch cost?", lets us set fee-bump limits deliberately, and stops
a runaway loop before it drains the operating account. All amounts are
**stroops** (1 XLM = 10^7 stroops).

## Cost taxonomy

**Operation types:** `submission`, `fee_bump`, `restore_footprint`,
`extend_ttl`, `storage_write`, `upload_wasm`.

**Business actions** (these are also the budget categories):
`settlement_batch`, `vault_deposit`, `oracle_publish`, `keeper_run`,
`emergency_pause`, `admin`, `other`.

**Attribution:** every event carries `action` and `action_id` (the batch,
deposit or job id), plus optional `request_id`, `job_id`, `account` and
`series`. Reports can group by any of these, or by time window.

**Net cost.** Each event stores `inclusion_fee`, `resource_fee`, `rent_fee`,
`refund` and `net_cost`:

| Status | What is charged |
|---|---|
| `success` | inclusion + non-refundable + consumed refundable (rent/TTL, events, return value). The unconsumed refundable fee is refunded. |
| `failed_on_chain` | inclusion + non-refundable. The whole refundable fee is refunded. |
| `preflight_rejected`, `not_included` | nothing. The event is still recorded, at zero cost. |

If the authoritative `feeCharged` from the transaction result disagrees with
the computed breakdown, `net_cost` uses `feeCharged` and the event is marked
`reconciled = 0`. If the fee figures are inconsistent (e.g. more refundable
fee consumed than declared), the attempt is recorded at the full bid and
the error is returned to the caller. A cost is never silently dropped.

**Fee bumps.** The outer envelope pays the whole fee. Record a single
`fee_bump` event for the outer transaction and nothing for the inner one, so
the cost is never counted twice.

## Budget policy

There is one policy per category (`chain_budget_policies`): a daily or
weekly period (UTC; weeks start on Monday), a limit, a warn threshold
(basis points of the limit, default 80%), and a warn-state throttle.

| State | Committed (spent + in-flight) | Non-critical traffic | Critical traffic |
|---|---|---|---|
| `ok` | below warn | allowed | allowed |
| `warn` | warn ≤ committed < limit | throttled to `throttle_per_minute` | allowed |
| `hard_stop` | would exceed limit | **rejected** (`GuardrailError::HardStop`) | allowed, recorded as `critical_overrun` |

Critical traffic means settlement completion and emergency pause (the
defaults for `settlement_batch` and `emergency_pause`). A caller can also
mark a single submission as critical. A budget never blocks critical
traffic. A category with no policy is unbudgeted: spend is tracked, but
nothing is enforced.

**Durability and concurrency.** Counters live in `chain_budget_periods`,
so they survive restarts. `admit` reserves the fee bid with a single
conditional `UPDATE` inside a write transaction. Parallel workers and
multiple processes therefore cannot overshoot the limit; a test runs 64
racing workers to confirm it. `complete` turns the reservation into
actual spend. `CostLedger::expire_stale` releases reservations left behind
by a worker that died mid-submission; run it from the keeper.

**Loud breaches.** The first warn, hard stop or critical overrun in each
period:

- inserts a row into `chain_budget_breaches`,
- logs at `ERROR` with category, committed amount and limit,
- increments `zenith_chain_budget_breaches_total`.

Every rejection also increments `zenith_chain_budget_rejections_total`.
Alert on `zenith_chain_budget_state >= 1`.

## Integration

```rust
let admitted = chain_cost::admit(&ledger, OperationType::Submission,
    Attribution::new(BusinessAction::KeeperRun, job_id), None, max_fee_bid, now_ms).await?;
let result = submit(tx).await;
chain_cost::complete(&ledger, &admitted, status, tx_hash, ledger_seq, &fee_report, now_ms).await?;
```

TTL extensions and footprint restores use the same two calls, with
`OperationType::ExtendTtl` / `RestoreFootprint`. If a submission is
abandoned before sending, call `ledger.release(&admitted.reservation, now)`.

## Reporting

Mount `chain_cost::report::router(ledger)` under `/admin/chain-costs`,
behind the admin RBAC layer:

| Route | Returns |
|---|---|
| `GET /settlements/{batch_id}` | cost per settlement batch, by operation, with failed and unreconciled counts |
| `GET /daily?from_ms&to_ms` | cost per UTC day (from the pre-computed daily rollup) |
| `GET /operations?from_ms&to_ms` | cost distribution by operation type |
| `GET /grouped?from_ms&to_ms&dimension=action\|account\|series` | cost per action, account or series |
| `GET /budgets` | budget utilisation and state for the current period |
| `GET /metrics` | Prometheus text: `zenith_chain_budget_utilization_ratio`, `zenith_chain_budget_state`, `zenith_chain_cost_stroops_total`, `zenith_chain_budget_rejections_total`, `zenith_chain_budget_breaches_total` |

Out of scope: fiat conversion, invoicing, and the on-chain fee-accrual and
treasury sweep.
