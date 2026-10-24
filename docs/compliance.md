# Sanctions / AML screening

Paper trading treats every authenticated wallet as trusted. Once value
moves through Soroban escrow, every flow that moves funds must be screened
first. `src/compliance.rs` provides the gate; this page says how to run it.

## What is screened

| Flow            | `Flow` variant  | Gate before…                                   |
|-----------------|-----------------|------------------------------------------------|
| Deposit         | `Deposit`       | crediting escrow (including the on-chain cutover) |
| Withdrawal      | `Withdrawal`    | releasing funds from escrow                    |
| Open position   | `OpenPosition`  | locking premium or collateral                  |
| Settlement      | `Settlement`    | paying out an expired position                 |

Call `ComplianceGate::require(wallet, flow, now)` at the start of each of
these. It returns `Err(ComplianceError::Denied)` when the flow must not
proceed; that error renders as **HTTP 451** with a generic body
(`code: compliance_denied`). The reason for a denial (which list matched,
the risk score) is written to the audit table and is **never** sent to the
client, so the subject of a hit is not tipped off.

## Modes (`ZENITH_COMPLIANCE_MODE`)

| Mode      | Screens | Records | Blocks | Use                                      |
|-----------|---------|---------|--------|------------------------------------------|
| `off`     | no      | no      | no     | local development only                   |
| `monitor` | yes     | yes     | no     | default; paper trading, tuning thresholds |
| `enforce` | yes     | yes     | yes    | required before any on-chain settlement  |

An unknown value is a startup error, so a typo cannot quietly turn
enforcement off.

Under `enforce` the gate **fails closed**: if the provider times out or
errors, the flow is denied and the decision is recorded with
`source = 'unavailable'`. A missing answer is never treated as clear.

## Decision order

1. **Operator override.** The latest unexpired, unrevoked override for the
   wallet wins (`allow` for a confirmed false positive, `deny` for a manual
   freeze). Overrides require an operator and a reason.
2. **Cached screening.** A result from the same provider newer than
   `ZENITH_COMPLIANCE_CACHE_TTL_SECS` (default 24h) is reused.
3. **Fresh screening.** The provider is called and the result stored.

The effective verdict is the provider's verdict escalated by score:
`score >= ZENITH_COMPLIANCE_BLOCK_SCORE` (default 80) is `blocked`,
`score >= ZENITH_COMPLIANCE_REVIEW_SCORE` (default 50) is `review`. Under
enforcement both `review` and `blocked` deny; `review` cases are cleared by
an operator issuing an `allow` override after investigation.

## Providers

* `DenylistProvider` — a local JSON list, e.g. built from the OFAC SDN
  digital-currency address export:

  ```json
  { "version": "ofac-sdn-2026-10-20",
    "entries": [ { "address": "G...", "reason": "OFAC SDN: <program>" } ] }
  ```

  Set `ZENITH_SANCTIONS_LIST_PATH` to its location. Bump `version` on every
  refresh — it is stored with each screening so you can tell which list a
  past decision used.
* `HttpScreeningProvider` — a vendor API (`ZENITH_SCREENING_URL`, bearer
  token in `ZENITH_SCREENING_TOKEN`). Request and response shape are
  documented on the type.
* `CompositeProvider` — runs several providers; the worst verdict wins and
  any provider error fails the whole screening.

## Audit trail

| Table                    | Contents                                               |
|--------------------------|--------------------------------------------------------|
| `compliance_screenings`  | raw provider results (also the cache)                  |
| `compliance_overrides`   | operator overrides, with reason, expiry and revocation |
| `compliance_decisions`   | every gate decision; **append-only** (triggers reject UPDATE/DELETE) |

Useful queries:

```sql
-- Everything decided for one wallet, newest first.
SELECT created_at, flow, outcome, enforced, source, verdict, reason
FROM compliance_decisions WHERE wallet_address = ? ORDER BY id DESC;

-- What monitor mode would have blocked in the last 7 days.
SELECT wallet_address, flow, reason FROM compliance_decisions
WHERE outcome = 'deny' AND enforced = 0
  AND created_at > strftime('%s','now') - 7 * 86400;
```

## Before switching to `enforce`

1. Run in `monitor` and review every `outcome = 'deny'` row.
2. Clear false positives with time-limited `allow` overrides.
3. Confirm the provider's error rate. Every outage becomes a denial
   once enforcement is on.
4. Switch to `enforce` **before** the on-chain cutover (see
   `docs/runbooks/onchain-cutover.md`). The cutover screens every wallet
   before crediting escrow.
