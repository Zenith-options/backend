# Disaster recovery runbook

What state the settlement layer depends on, how each piece is backed up,
and how to bring the service back after losing it. The step-by-step
Litestream point-in-time restore lives in [`restore.md`](restore.md); this
document covers everything around it.

## Targets

| | Target | How it's met |
|---|---|---|
| **RPO** (data loss) | < 10 s | Litestream streams WAL frames continuously |
| **RPO**, replica lost | ≤ snapshot interval (default 1 h) | `scripts/backup.sh` snapshots to a separate bucket |
| **RTO** (back in service) | < 30 min | restore + validation is seconds; most time is indexer catch-up and the go-live checklist |

The RPO only bounds what the database loses. Anything that happened on
chain after the backup point is still recoverable from the chain itself —
see [Recovering on-chain state](#4-recover-on-chain-state).

## Inventory of durable state

| State | Where it lives | Source of truth | Backed up by | After restore |
|---|---|---|---|---|
| Accounts, positions, ledger, fixings, liquidations, reserve | `/data/zenith.db` | the database | Litestream + snapshots | validated by `restore.sh` |
| Indexer events (`chain_events` + projections) | `/data/zenith.db` | the chain | Litestream + snapshots | rebuilt by backfill from the restored cursor |
| Indexer cursors / checkpoints (`indexer_cursors`, `indexer_checkpoints`) | `/data/zenith.db` | the database (positions on chain) | Litestream + snapshots | plan the catch-up with `dr::plan_indexer_recovery` |
| Stellar source-account sequence numbers | on chain | **the chain** | not backed up on purpose | re-read from Horizon with `dr::resolve_sequence` |
| Signing keys, HMAC keys, DB credentials | Vault / KMS / SOPS | secret store | the secret store's own backup | see [`secrets-rotation.md`](../secrets-rotation.md) |
| Archived cold data (`archives/`, `archive_manifests`) | object storage | the archive files | object-storage versioning | `archive_manifests` restored with the DB |

Everything in the first four rows is one SQLite file, so a consistent copy
of that file is a consistent copy of the settlement layer.

## Backup layers

1. **Continuous replication (Litestream).** Configured in
   `deploy/litestream.yml`; WAL frames go to object storage within seconds.
   Point-in-time restore to any moment inside the retention window.
2. **Periodic snapshots (`scripts/backup.sh`).** An online `.backup` copy
   (safe while the backend writes), integrity-checked, gzipped, with a
   `.manifest.json` that records the sha256, migration version, indexer
   cursors and row counts of the settlement tables. Run it hourly from cron
   or a Kubernetes `CronJob`:

   ```bash
   BACKUP_S3_URI=s3://zenith-snapshots/prod scripts/backup.sh /data/zenith.db /data/backups
   ```

   Upload to a **different bucket and credential** than Litestream, with
   object lock / versioning on. The point of this layer is to survive losing
   or corrupting the Litestream replica.

3. **Nightly drill.** `.github/workflows/backup-drill.yml` restores from
   Litestream and from a snapshot and runs every validation check. A backup
   that has never been restored is not a backup.

## Recovery procedure

### 1. Stop writers

Scale the backend and keeper to zero. Two writers against a restored file —
or a keeper submitting transactions with a stale sequence number — is how a
recoverable incident becomes an unrecoverable one.

### 2. Restore the database

Pick the source:

```bash
# Litestream, point in time (normal case)
scripts/restore.sh 2026-10-21T12:34:56Z /tmp/zenith-restore.db

# Snapshot (Litestream replica lost or corrupt)
scripts/restore.sh /data/backups/zenith-20261021T120000Z.db.gz /tmp/zenith-restore.db
```

`restore.sh` refuses to report success unless the snapshot checksum matches
its manifest, `PRAGMA integrity_check` passes, the migration state is
sane, and the ledger reconciliation invariant holds. It then prints the
restored indexer cursors and any unresolved gap alerts.

### 3. Plan the indexer catch-up

The restored cursors point at the backup moment. Before trusting settlement
outcomes again, every ledger from there to the chain tip must be
re-ingested, plus any holes the checkpoint table already had.

`dr::load_indexer_state` + `dr::plan_indexer_recovery` turn the restored
checkpoints and cursors into a plan:

- `streams[].resume_from` — the first ledger each stream must ingest;
- `backfill[]` — sorted, merged `[from, to)` ranges, each tagged `rpc` or
  `ledger_meta_archive`. Ranges older than the RPC retention window
  (`DEFAULT_RPC_RETENTION_LEDGERS`, ~24 h) can only come from the LedgerMeta
  archive;
- `warnings[]` — e.g. a cursor ahead of the chain tip (backup from the
  wrong network) or past the last checkpoint. **Resolve every warning
  before restarting the indexer.**

Ingestion is idempotent (`chain_events` is unique on
`(stream, paging_token)`), so re-running an overlapping range is safe.

### 4. Recover on-chain state

- **Sequence numbers.** Never reuse a sequence number from the database.
  For each source account, read the current sequence from Horizon and pass
  it through `dr::resolve_sequence(backup, on_chain)`. It always returns the
  on-chain value; a positive `drift` means transactions landed after the
  backup point and their effects will come back through the indexer
  backfill. A `BackupAheadOfChain` error means the backup belongs to a
  different network (or testnet was reset) — stop and investigate.
- **In-flight transactions.** Any transaction submitted but not confirmed
  before the incident is either on chain (the backfill will record it) or
  expired. Don't resubmit until the backfill has passed its ledger.

### 5. Go-live checklist

- [ ] `restore.sh` printed `PASS`
- [ ] Indexer plan has no warnings; all backfill ranges complete
- [ ] `indexer_gap_alerts` has no unresolved rows
- [ ] Sequence numbers resolved from chain for every signer
- [ ] `GET /health` and the readiness endpoint are green
- [ ] Keeper restarted **last**, after the indexer reached the tip

Record the restore source, timestamp, drift per signer and total time in
the incident log; paste the RTO into the table in [`restore.md`](restore.md).

## Scenarios

| Scenario | Action |
|---|---|
| Container / node lost, volume intact | Restart. SQLite in WAL mode recovers on open. |
| Volume lost | Litestream restore to `now`, then steps 3–5. |
| Logical corruption (bad migration, bad deploy, wrong writes) | Litestream restore to just **before** the bad event; steps 3–5. Replayed chain events re-apply anything legitimate. |
| Litestream replica lost or corrupt | Restore the newest snapshot; steps 3–5. Re-seed Litestream from the restored file. |
| Region lost | Restore in the standby region from the replica (or snapshot bucket, which should be cross-region replicated); steps 3–5. |
| Secrets lost | Recover from the secret store's backup; see `secrets-rotation.md`. The database never held private keys. |
| Wrong-network backup restored | `resolve_sequence` and the indexer plan both flag it; stop and pick the right backup. |
