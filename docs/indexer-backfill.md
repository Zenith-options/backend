# Indexer backfill and gap detection

The live indexer follows the chain tip. It is fast, but it is not
indestructible: a restart, an RPC outage or a cursor regression can leave
holes in the ledgers it has processed. A silent hole is worse than a failed
transaction — positions, collateral or settlements that exist on-chain never
appear off-chain, and reconciliation, balances and reporting are quietly
wrong until a user complains.

This document describes how the holes are detected, how they are repaired, and
what the RPC's retention window means for the repair.

## Coverage tracking

Every range of ledgers the indexer has processed is recorded in
`indexed_ledger_ranges` as a half-open range `[from_ledger, to_ledger)` with
the source that produced it (`live` or `backfill`).

`detect_gaps(ranges, tip)` turns that coverage into the list of missing
ranges. Ranges are merged first, so a range that is covered twice (a replay
that overlapped the live indexer) does not look like a hole, and two ranges
that touch do not produce a gap between them.

The depth of the open gaps — the total number of missing ledgers — is
published as a metric and as a readiness signal
(`IndexerReadiness::ready` is false while a gap is open).

## The shared projection

The projection step is a pure function over a decoded event batch:

```
project_events(&[DecodedChainEvent]) -> Vec<ProjectionRow>
```

Both the live tip-following loop and the historical replay call it, so a
replayed range produces exactly the same rows as the original pass. Nothing
about the projection depends on where the events came from.

Rows are keyed by **transaction hash plus event index**
(`EventKey`), and the database enforces the same key
(`UNIQUE (stream, tx_hash, event_index)`) with `ON CONFLICT DO NOTHING`.
Replaying an already indexed range therefore writes the same rows again and
changes nothing: the projection state after a replay is byte-identical to the
state before it.

## The repair

```
gaps ──► plan_backfill() ──► BackfillRunner::run() ──► same projection step
             │
             └──► ranges outside the retention window are reported as skipped
```

`plan_backfill` slices the gaps into ranges of at most
`ZENITH_INDEXER_MAX_RANGE` ledgers (1 000 by default) so one huge gap is
replayed in slices instead of one enormous request.

`BackfillRunner::run` replays the ranges in ascending order, so the
projection always moves forward. It pages through the events of a range
(`ZENITH_INDEXER_PAGE_LIMIT` ledgers per request,
`ZENITH_INDEXER_RATE_LIMIT_MS` pause between requests) and writes through
the same projection step as the live indexer. A range that fails is reported
in the result and the run continues: one bad range must not stop the repair.

## The retention limitation

**Soroban RPC nodes only serve events for a bounded window of ledgers.** A
range older than that window can never be replayed — the node does not have
the events any more.

`BackfillRunner::detect_retention_floor` finds the oldest ledger the node
still serves with a binary search (the retention window is monotonic, so the
search is logarithmic in the number of ledgers). Two consequences:

* a gap that starts *before* the floor is **not** silently skipped: it is
  reported as `SkipReason::OutsideRetention` with the floor, so the operator
  sees exactly how much of the gap is unrecoverable;
* the part of a gap that is *inside* the window is still replayed.

This is an operational limitation, not a bug: a node that has forgotten an
event cannot be asked for it again. The durable fix is to archive the events
the platform needs before the node drops them, which is out of scope here.

## Metrics and readiness

`IndexerMetrics` counts what the indexer did:

| Counter | Meaning |
|---|---|
| `events_indexed` | events written to the store |
| `ranges_indexed` | ledger ranges written to the store |
| `gaps_detected` | number of open gaps |
| `gap_depth_ledgers` | total number of missing ledgers |
| `backfilled_ranges` | ranges replayed by a backfill |
| `backfilled_ledgers` | ledgers replayed by a backfill |
| `backfilled_events` | events replayed by a backfill |
| `replay_failures` | replays that failed |

`IndexerReadiness` turns the same information into a signal: it is `ready`
when the indexer caught up with the tip and no gap is open, and it carries
the reason when it is not.

## Operator runbook

1. The readiness signal reports a gap (`gap_depth_ledgers > 0`).
2. Find the open gaps:
   `SELECT * FROM indexer_ledger_gaps WHERE resolved_at IS NULL;`
3. Find the retention floor of the node the platform talks to, and compare it
   with the oldest gap.
4. Replay the repairable ranges through `BackfillRunner`. The plan reports
   the ranges it will replay and the ranges it refuses to replay, with the
   reason.
5. Check `indexer_backfill_runs` for the outcome of every replay, and
   `indexer_ledger_gaps` for the gaps that are still open.
