# Insurance reserve and deficit ledger

Liquidations can fail to recover an account's full shortfall when markets
move fast or are illiquid. That gap has to land somewhere. Without an
explicit ledger it turns into a negative balance nobody reports, and every
downstream accounting report is wrong. `src/insurance.rs` and migration
`0012_insurance_reserve.sql` make it explicit.

## Model

All amounts are integer stroops (1e-7) of the settlement asset.

- **Reserve** (`insurance_reserve_state`, `insurance_reserve_entries`). One
  balance plus an append-only journal (UPDATE/DELETE are rejected by
  triggers). Inflows: `contribution`, `liquidation_penalty`, `fee_share`.
  Outflows: `deficit_cover`, `withdrawal`. Every entry records
  `balance_after`, and the balance can never go negative.
- **Deficits** (`liquidation_deficits`). One row per liquidation that ended
  with a shortfall, keyed by `liquidation_ref` so the same liquidation can
  never be recorded (or covered) twice.
  `outstanding = shortfall - covered - written_off`. Status is `open` while
  anything is outstanding, then `covered` or `written_off`.

## Flow

```text
liquidation ends with equity < 0
        │
        ▼
record_shortfall(liquidation_ref, account, amount)
        │  cover = min(reserve, shortfall)        → deficit_cover entry
        ▼
outstanding > 0 ? ── no ──► deficit status = covered
        │ yes
        ▼
deficit stays open (warn log, visible in coverage report)
        │
        ├─ later inflows + cover_open_deficits()  → oldest first
        └─ governance write_off(deficit, memo)    → status = written_off
```

Withdrawals are refused while any deficit is open: the reserve exists to
cover those first.

## Coverage metric

`GET /api/v1/insurance/coverage` (public) returns:

| Field | Meaning |
|---|---|
| `reserve_balance` | Current reserve |
| `open_deficits`, `outstanding_deficit` | Unsettled shortfalls |
| `net_reserve` | `reserve_balance - outstanding_deficit`; negative = more liability than reserve |
| `at_risk_collateral` | Collateral locked by open short positions |
| `coverage_ratio_bps` | `net_reserve / at_risk_collateral` in basis points |
| `recovery_rate_bps` | Share of all recorded shortfalls the reserve covered |
| `total_shortfall`, `total_covered`, `total_written_off` | Lifetime totals |

`coverage_ratio_bps` is the number to publish: how much of the liability
base the reserve can absorb right now, net of what it already owes.

## Reconciliation

`insurance::verify(pool)` returns every violation of:

- reserve balance = sum of the journal = last entry's `balance_after`;
- each deficit's `covered` = the cover draws journaled against it.

Run it in the nightly reconciliation job and alert on any non-empty
result.

## Concurrency

Each mutating transaction writes before it reads, so it holds SQLite's
write lock before reading the balance it spends. Twenty concurrent
shortfalls against a reserve that can only cover ten cover exactly ten
(see `concurrent_shortfalls_never_overdraw_the_reserve`).
