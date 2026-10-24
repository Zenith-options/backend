# Shadow-mode settlement

Cutover from paper trading to live on-chain settlement is the highest-risk
moment in the programme. Shadow mode produces evidence, before any real
value moves, that the on-chain path makes the same margin, payoff and
expiry decisions as the off-chain engine. After cutover it keeps running as
a regression detector for contract upgrades.

## Modes

Set with `SETTLEMENT_MODE` (unset = `paper`; any other value fails startup
instead of silently defaulting):

| Mode | Authoritative | On-chain path | Compared |
|---|---|---|---|
| `paper` | off-chain engine | not called | no |
| `shadow` | off-chain engine | **simulated only**, never submitted | yes |
| `live` | on-chain contract | submitted | yes, against the off-chain engine |

## How it works (`src/shadow.rs`)

1. The off-chain engine makes a decision (margin required, payoff, or
   expiry outcome) and hands it to `ShadowRunner::observe` as a `Decision`
   with the inputs it used.
2. The runner calls `OnChainPath::simulate` — in production a Soroban
   `simulateTransaction` of the matching contract call — with a timeout.
3. `compare` classifies the pair:
   - `match` — identical;
   - `within_tolerance` — amounts differ by no more than the tolerance
     (default 10 stroops, for f64-vs-integer rounding);
   - `diverged` — beyond tolerance, or **any** difference in the expiry
     exercise decision;
   - `on_chain_error` — the simulation failed or timed out.
4. Every comparison goes to `shadow_comparisons` with the contract version
   (wasm hash), both results, the diff and the JSON inputs, so any
   divergence can be replayed. Divergences are also logged at `warn`.

The shadow path can't hurt the authoritative one: `observe` never returns
an error, simulation is time-boxed, and a failure to persist is logged, not
raised.

## Cutover gate

`cutover_readiness(pool, &CutoverCriteria::strict(wasm_hash))` reports per
decision kind: samples, matches, divergences, errors, divergence rate and
the largest diff, plus a list of blockers. The strict default requires, for
the **currently deployed** contract version:

- at least 500 comparisons of each kind (margin, payoff, expiry),
- within the last 7 days,
- with zero divergences and zero on-chain errors.

Evidence from an older contract version doesn't count: upgrading the
contract restarts the clock.

### Cutover checklist

- [ ] `SETTLEMENT_MODE=shadow` in production for at least the window
- [ ] `cutover_readiness(...).ready == true` for the wasm hash about to go live
- [ ] Every historical divergence explained (query below) and fixed or accepted
- [ ] Insurance reserve funded (see `insurance-reserve.md`)
- [ ] Backups and the restore drill green (see `runbooks/disaster-recovery.md`)
- [ ] Switch to `SETTLEMENT_MODE=live`; keep watching divergences

## Investigating divergences

```sql
SELECT observed_at, kind, subject_id, off_chain_amount, on_chain_amount,
       off_chain_exercised, on_chain_exercised, diff, detail, inputs
  FROM shadow_comparisons
 WHERE verdict IN ('diverged', 'on_chain_error')
 ORDER BY observed_at DESC
 LIMIT 50;
```

`inputs` holds everything needed to re-run the decision on both paths.
Comparison rows are operational evidence and fall under the retention
policy in `data-retention.md`.
