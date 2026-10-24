# Data retention and archival

Once the event indexer and backfill run against real ledgers, the SQLite
database grows without bound, which slows queries and backups. Regulated
settlement also needs historical records retained long-term — but not hot.
`src/retention.rs` moves expired rows out of the database into checksummed
cold archives, and never deletes a row it hasn't durably archived first
(unless the policy explicitly opts out of archiving).

## Table classes

| Class | Tables | Rule |
|---|---|---|
| **Protected** (`PROTECTED_TABLES_DENYLIST`) | accounts, positions, ledger, settlement fixings, liquidation deficits, insurance reserve, indexer cursors/checkpoints, archive manifests, migrations, sponsorship/wasm/network metadata | Never purged. The settlement record of truth stays hot. |
| **Archive-required** (`ARCHIVE_REQUIRED_TABLES`) | `chain_events` and its projections, `liquidations`, `shadow_comparisons` | May leave the hot DB only into an archive; a policy with `archive = false` is rejected. |
| **Ephemeral** | e.g. `keeper_job_executions` | May be purged without archiving. |

## Default policies (`default_policies()`)

| Table | Age column | Hot for | Archived |
|---|---|---|---|
| `series_created`, `option_minted`, `option_exercised`, `collateral_deposited`, `settled` | `created_at` | 180 days | yes |
| `chain_events` | `ingested_at` | 180 days | yes |
| `shadow_comparisons` | `observed_at` | 90 days | yes |
| `price_candles` | `minute` | 365 days | yes |
| `keeper_job_executions` | `created_at` | 30 days | no |

Projections come before `chain_events` because they reference it: a batch
whose rows are still referenced by another table is refused (deleting it
would cascade into rows that were never archived). Tables missing from the
schema are skipped.

The hot window must stay longer than the RPC retention window plus the
longest gap you'd expect to backfill, so the indexer never needs to re-read
archived data to resume (see `runbooks/disaster-recovery.md`).

## How a batch is archived

1. `BEGIN IMMEDIATE`, select up to `batch_size` (max 10 000) expired rows,
   oldest first.
2. Refuse if any child table still references them.
3. Write the rows as JSON lines (BLOBs as `{"$hex": "…"}`) to
   `$ARCHIVE_DIR/<table>/<table>_<oldest date>_<manifest id>.jsonl` via a
   temp file, `fsync`, rename, `fsync` the directory.
4. Insert an `archive_manifests` row (sha256, row count, oldest/newest age),
   delete exactly those rows, commit.

A crash before the commit leaves every row in place; the worst case is an
orphan archive file with no manifest, which the next run supersedes.
Timestamps must be ISO-8601 text (`YYYY-MM-DDTHH:MM:SS…`, as normalised by
the timestamp migration) or integer Unix seconds.

## Running it

```rust
let results = retention::run_policies(&pool, &retention::default_policies(), &archive_root).await;
// or, as a background task (run on one instance only, e.g. behind the keeper lease):
tokio::spawn(retention::retention_loop(pool, retention::default_policies(), archive_root, Duration::from_secs(3600)));
```

`ARCHIVE_DIR` (default `archives/`) is where `execute_purge_policy` writes.

## Long-term storage

The archive directory is the staging area, not the long-term store. Sync it
to object storage with **object lock (WORM) in compliance mode** for the
regulatory retention period (commonly 5–7 years), and a lifecycle rule that
moves objects to an archive storage class after 30 days:

```bash
aws s3 sync "$ARCHIVE_DIR" s3://zenith-archive/prod/ --only-show-errors
```

`archive_manifests` stays in the hot database (it is protected), so it is
always possible to find which file holds a given table and time range.

## Auditing and retrieval

- `verify_archive(pool, root, manifest_id)` re-reads a file and checks its
  sha256 and row count against the manifest. Run it on a sample nightly.
- `restore_archive(pool, root, manifest_id)` verifies, then loads the batch
  back into its table (`INSERT OR IGNORE`) and stamps
  `archive_manifests.restored_at` — for audits and disputes.

## Reclaiming space

Deleting rows frees pages for reuse but doesn't shrink the file. Run
`VACUUM` in a maintenance window after a large first archival (it needs
free disk equal to the database size), and take a fresh snapshot with
`scripts/backup.sh` afterwards.
