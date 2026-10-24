# Soroban fees: footprint, resource fee and inclusion fee

A Soroban transaction pays for two different things, and the platform has to
get both right:

| Fee | What it pays for | Where it comes from |
|---|---|---|
| **Resource fee** | the CPU instructions, the ledger bytes read and written, and the events the host executes | the resources the simulation declared |
| **Inclusion fee** | a slot in a ledger, i.e. being included before the transaction's `validUntilLedger` | the fee strategy, from the fees recently paid on the network |

Getting either of them wrong has a cost:

* a **static inclusion fee** is dropped during congestion, so a settlement
  lands late and the user stays exposed; or it systematically overpays, which
  is a recurring cost on every single settlement;
* a **hard-coded resource fee** under-pays on a transaction that touches more
  state than the constant assumed, and the transaction is rejected by the
  network with `tx_insufficient_fee`.

This document describes how the fees are chosen, what the defaults are, and
how to audit what was paid.

## The flow

```
TxBuildRequest
      │
      ▼
  simulate()                    ← src/chain/simulate.rs
      │  ledger footprint, declared resources, minResourceFee
      ▼
  resource_fee_for()            ← resource fee from the declared resources
      │
      ▼
  FeeOracle::fee_stats()        ← src/chain/fees.rs
      │  recent inclusion-fee distribution
      ▼
  InclusionFeeStrategy::select()  ← percentile, floor, ceiling, per-tx cap
      │
      ▼
  DailyFeeBudget::try_reserve()   ← rolling 24h budget
      │
      ▼
  TransactionEnvelope            ← footprint + both fees, ready to sign
```

## The footprint

The ledger footprint (the entries the transaction reads and writes) is only
known to the host, so it is taken from `simulateTransaction` and attached to
the envelope. It is also hashed (`footprint_hash`, a SHA-256 over the read and
write keys, in the order the host reported them) and stored with the
transaction record, so an auditor can tell whether two transactions declared
the same state without the footprint itself being persisted.

A footprint is only valid for a short window. When the chain advances past
`simulated_ledger + DEFAULT_FOOTPRINT_TTL_LEDGERS` (2 ledgers by default), the
simulation is repeated; the transaction is only built once the footprint is
stable.

## The resource fee

The resource fee is computed from the resources the simulation declared:

```
resource_fee = max(minResourceFee,
                    instructions * fee_per_instruction
                  + read_bytes    * fee_per_read_byte
                  + write_bytes   * fee_per_write_byte
                  + events        * fee_per_event)
```

The price sheet (`ResourcePricing`) is in stroops per unit and defaults to 1
stroop per unit. `minResourceFee` — the minimum the node itself charges — is
always a lower bound, so a mis-calibrated price sheet can only ever
over-collect, never under-collect.

## The inclusion fee

`InclusionFeeStrategy` bids a percentile of the inclusion fees the network
recently accepted:

| Parameter | Env var | Default | Meaning |
|---|---|---|---|
| `percentile` | `ZENITH_FEE_PERCENTILE` | 90 | which percentile of the recent distribution to bid |
| `floor_stroops` | `ZENITH_FEE_FLOOR_STROOPS` | 100 | lower bound of the bid; below this the transaction risks being dropped |
| `ceiling_stroops` | `ZENITH_FEE_CEILING_STROOPS` | 100 000 (0.1 XLM) | upper bound of the bid |
| `per_transaction_cap_stroops` | `ZENITH_FEE_PER_TX_CAP_STROOPS` | 500 000 (0.5 XLM) | upper bound of the **total** fee (resource + inclusion) for one transaction |
| `daily_budget_stroops` | `ZENITH_FEE_DAILY_BUDGET_STROOPS` | 5 000 000 (5 XLM) | rolling 24h budget for inclusion fees |

The selection is:

1. take `stats.percentile(percentile)` from the recent fee sample;
2. clamp it into `[floor, ceiling]`;
3. if `resource_fee + bid > per_transaction_cap`, reduce the bid to the
   remaining headroom (the bid is then reported as `capped`);
4. if the resource fee alone is above the cap, the transaction is refused —
   the platform does not sign a transaction it is not willing to pay for.

**When the node does not serve fee statistics** (older testnet nodes do not
implement `getFeeStats`), the bid falls back to the floor instead of failing
the submission. The reason for the bid is recorded as `floor` in the audit
trail.

**The percentile is the default, not a constant.** A p90 bid is included
without paying the p99, and it is still high enough to be included in a
congested ledger. Operators who see settlements land late can raise the
percentile; operators who see the protocol overpaying can lower it.

## The daily budget

`DailyFeeBudget` is a rolling 24 hour window over the inclusion fees that
were reserved. A reservation is taken before the envelope is returned; if the
transaction is not submitted, the reservation is released again. Once the
window is exhausted, the builder refuses the transaction with
`BudgetExceeded` — the protocol stops paying rather than silently
overspending.

## Auditing

Every built transaction records:

| Column | Meaning |
|---|---|
| `chain_txs.inclusion_fee` | the inclusion fee that was bid, in stroops |
| `chain_txs.resource_fee` | the resource fee, in stroops |
| `chain_txs.footprint_hash` | the hash of the declared footprint |
| `chain_txs.fee_source` | `percentile`, `floor` or `capped` |
| `chain_txs.fee_percentile` | the percentile that was bid, if any |
| `chain_tx_fees.*` | the same decision, kept in full for the audit trail |

`record_fee_audit` writes both.

## Operational notes

* The fee statistics are polled in the background
  (`ZENITH_FEE_POLL_INTERVAL_SECS`, 15s by default) and served from a cache
  (`ZENITH_FEE_CACHE_TTL_SECS`, 30s). A failed refresh keeps the previous
  sample.
* A transaction that is built but never submitted should release its budget
  reservation, otherwise the daily budget is consumed by transactions that
  never reached the network.
* The raw simulation envelope and the invocation arguments are never logged
  and never persisted: they carry the full payload of the transaction. Only
  the shape of the transaction (ledger, entry counts, fees) is logged.
* Fee-bump transactions and sponsorship are out of scope here: they are
  handled by the signing/submission work and by the fee-bump relayer.
