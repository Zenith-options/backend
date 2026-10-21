# On-chain settlement cutover runbook

Moves every user's off-chain paper balance and locked collateral into the
Soroban escrow contract, once, without stranding or deleting anyone, and
proves the books match afterwards. Implementation: `src/cutover.rs`.
Schema: `migrations/0013_onchain_cutover.sql`.

## Guarantees

| Property | How |
|---|---|
| **Nobody is deleted** | The cutover never modifies or removes `accounts` or `positions` rows. Verification fails if any snapshot wallet's account row is missing. |
| **Nobody is stranded** | Every wallet ends `migrated` (funds on-chain) or `held` (funds stay off-chain, visible, recoverable). `verify` refuses to pass while any wallet is `pending` or `failed`. |
| **Balances match exactly** | Amounts are integer stroops. `verify` compares escrow against the immutable snapshot with zero tolerance and checks `snapshot = on-chain + held`. |
| **Resumable** | Per-wallet progress is recorded after every credit, and credits use idempotency keys (`<run>:<wallet>:free` / `:collateral`). Re-running `migrate` resumes. It never double-credits. |
| **Reversible** | `rollback` debits exactly what was recorded as credited and returns every wallet to `offchain`. The untouched off-chain rows become authoritative again. |
| **Right contract** | `migrate` calls `PinGuard::ensure_live` on the escrow before crediting (see `docs/contract-builds.md`). |
| **Screened** | Each wallet passes `ComplianceGate` (`Flow::Deposit`) before credit. Denied wallets are `held` (see `docs/compliance.md`). |
| **Auditable** | `cutover_events` is append-only and the snapshot table is immutable (triggers). |

## States

```
run:     snapshotted ─▶ migrating ─▶ migrated ─▶ verified
              └────────────┴────────────┴───────────┴──▶ rolled_back

wallet:  pending ─▶ migrated | held | failed ─(re-run migrate)─▶ migrated
                 └──────────────── rollback ────────────────▶ reverted

account_settlement_mode:  offchain ─snapshot─▶ migrating ─▶ onchain
                                                  └─held / rollback─▶ offchain
```

Only one run can be in flight (`snapshotted`, `migrating`, `migrated`) at a
time. This is enforced by a unique index.

## Prerequisites

- [ ] `ZENITH_COMPLIANCE_MODE=enforce` in the target environment.
- [ ] The escrow contract is pinned on the target network with a
      `contract_id`, and `scripts/contracts.sh verify-deployed <network>`
      passes.
- [ ] Trading handlers reject writes for wallets whose
      `settlement_mode()` is `migrating`. The snapshot freezes wallets by
      setting this mode, and drift detection catches any write that slips
      through. Drift detection is the backstop, not the control.
- [ ] The production `Escrow` implementation's `credit`/`debit` are
      idempotent on the supplied key (replay returns the original tx).
- [ ] At least **two successful rehearsals** on copies of production data
      taken within the last 7 days (below).

## 1. Rehearse (repeat until boring)

1. Take a consistent copy of the live DB. `copy_database(&pool, path)`
   runs `VACUUM INTO`, which is safe while the service is live. It refuses
   to overwrite an existing file and errors if the source is in-memory.
2. Point a `Cutover` at the **copy**, with an `InMemoryEscrow` whose
   contract ID is the pinned escrow ID.
3. Run `plan()` and fix every blocking issue **in production** (negative
   balances, collateral that does not match open positions, positions
   without an account). Then take a fresh copy and repeat.
4. Run `rehearse()`. It snapshots, migrates, verifies and rolls back, and
   `RehearsalReport::passed()` must be `true`. Record the report (JSON
   serialisable) and the plan's `sha256` in the change ticket.
5. Time the rehearsal. The production window is that time multiplied by
   the on-chain latency per credit, plus margin.

## 2. Production cutover

1. **Announce the window** and pause the settlement keeper and the
   settlement review queue (`docs/settlement-review.md`).
2. **Dry run**: `plan()` against production. Compare totals with the last
   rehearsal and explain any difference before continuing.
3. **Snapshot**: `snapshot(label, options, operator, now)`. Record the
   run ID and `snapshot_sha256`. From this moment every wallet in the
   snapshot is frozen (`migrating`).
4. **Migrate**: `migrate(run_id, operator, now)`. If it reports
   `complete: false`, read `cutover_wallets WHERE status = 'failed'`, fix
   the cause (escrow error, drift, non-empty escrow) and run `migrate`
   again. Repeat until complete.
5. **Verify**: `verify(run_id, operator, now)`. The run becomes `verified`
   only when the report is clean. Attach the report to the ticket.
6. **Held wallets**: list them with
   `SELECT wallet_address, detail FROM cutover_wallets WHERE run_id = ? AND status = 'held'`.
   Their funds remain in the off-chain ledger. Compliance resolves each
   case. A cleared wallet is migrated by a later, smaller run, because
   `plan()` skips wallets that are already on-chain.
7. Resume the keeper and the review queue.

## 3. Rollback

Roll back if `verify` cannot be made clean, or for any doubt before the
window closes:

```
rollback(run_id, operator, "<reason>", now)
```

It is idempotent. If it reports `complete: false`, fix the escrow-side
problem and run it again. When complete, escrow holds zero for every
snapshot wallet, every wallet is `offchain`, and the run is `rolled_back`.
A new run may then be started.

Rollback after the window closes, once users have traded on-chain, is
**not** this procedure. On-chain activity after the cutover is not in
the snapshot, and debiting the snapshot amounts would be wrong. That case
needs a forward fix.

## Useful queries

```sql
-- Progress of a run.
SELECT status, COUNT(*) FROM cutover_wallets WHERE run_id = ? GROUP BY status;

-- Full audit trail.
SELECT created_at, wallet_address, event, actor, detail
FROM cutover_events WHERE run_id = ? ORDER BY id;

-- Where does a user's money live right now?
SELECT mode, run_id FROM account_settlement_mode WHERE wallet_address = ?;
```
