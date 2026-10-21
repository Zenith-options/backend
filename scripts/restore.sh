#!/usr/bin/env bash
#
# Restore the Zenith database, followed by validation. Two sources:
#
#   * a Litestream replica (point-in-time), or
#   * a snapshot written by scripts/backup.sh (when the replica is lost).
#
# See docs/runbooks/restore.md and docs/runbooks/disaster-recovery.md.
#
# Usage:
#   scripts/restore.sh <target-timestamp | snapshot.db.gz> [output-path]
#
#   <target-timestamp>  RFC 3339 timestamp to restore to, e.g.
#                       2026-09-28T12:34:56Z (or "now" for the latest)
#   <snapshot.db.gz>    path to a backup.sh snapshot; its sibling
#                       .manifest.json must exist and its sha256 must match
#   [output-path]       where to write the restored database
#                       (default: /tmp/zenith-restore.db)
#
# Environment:
#   LITESTREAM_CONFIG   path to the litestream config
#                       (default: deploy/litestream.yml)
#   LITESTREAM_*        credentials matching the config (bucket, endpoint,
#                       region, access-key-id, secret-access-key)
#
# Requires: litestream (or docker) and sqlite3.

set -euo pipefail

TIMESTAMP="${1:?usage: restore.sh <target-timestamp | snapshot.db.gz> [output-path]}"
OUT="${2:-/tmp/zenith-restore.db}"
CONFIG="${LITESTREAM_CONFIG:-deploy/litestream.yml}"

SNAPSHOT=""
case "$TIMESTAMP" in
  *.db.gz) SNAPSHOT="$TIMESTAMP" ;;
esac

if [ -z "$SNAPSHOT" ] && [ ! -f "$CONFIG" ]; then
  echo "ERROR: litestream config not found at $CONFIG" >&2
  exit 1
fi

START_NS=$(date +%s%N)

# ── 1. Restore ──────────────────────────────────────────────────────────────
if [ -n "$SNAPSHOT" ]; then
  # Snapshot restore: refuse a file whose checksum doesn't match the
  # manifest written next to it at backup time.
  MANIFEST="${SNAPSHOT%.db.gz}.manifest.json"
  if [ ! -f "$SNAPSHOT" ] || [ ! -f "$MANIFEST" ]; then
    echo "ERROR: snapshot $SNAPSHOT or manifest $MANIFEST not found" >&2
    exit 1
  fi
  EXPECTED_SUM="$(sqlite3 :memory: "SELECT json_extract(readfile('$MANIFEST'), '\$.sha256');")"
  if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL_SUM="$(sha256sum "$SNAPSHOT" | cut -d' ' -f1)"
  else
    ACTUAL_SUM="$(shasum -a 256 "$SNAPSHOT" | cut -d' ' -f1)"
  fi
  if [ "$EXPECTED_SUM" != "$ACTUAL_SUM" ]; then
    echo "FAIL: snapshot sha256 $ACTUAL_SUM != manifest $EXPECTED_SUM" >&2
    exit 1
  fi
  echo "ok: snapshot checksum matches manifest"
  gunzip -c "$SNAPSHOT" > "$OUT"
elif command -v litestream >/dev/null 2>&1; then
  # Prefer the litestream binary; fall back to the official container.
  litestream restore -config "$CONFIG" -timestamp "$TIMESTAMP" -o "$OUT"
else
  docker run --rm \
    -e LITESTREAM_BUCKET="${LITESTREAM_BUCKET:-}" \
    -e LITESTREAM_ENDPOINT="${LITESTREAM_ENDPOINT:-}" \
    -e LITESTREAM_REGION="${LITESTREAM_REGION:-}" \
    -e LITESTREAM_ACCESS_KEY_ID="${LITESTREAM_ACCESS_KEY_ID:-}" \
    -e LITESTREAM_SECRET_ACCESS_KEY="${LITESTREAM_SECRET_ACCESS_KEY:-}" \
    -v "$(pwd)/$CONFIG:/litestream.yml:ro" \
    -v "$(dirname "$OUT"):/out" \
    litestream/litestream:v0.5.17 restore \
      -config /litestream.yml -timestamp "$TIMESTAMP" -o "/out/$(basename "$OUT")"
fi

if [ ! -f "$OUT" ]; then
  echo "ERROR: restore did not produce $OUT" >&2
  exit 1
fi
echo "restored database to $OUT"

# ── 2. PRAGMA integrity_check ───────────────────────────────────────────────
INTEGRITY="$(sqlite3 "$OUT" "PRAGMA integrity_check;")"
if [ "$INTEGRITY" != "ok" ]; then
  echo "FAIL: integrity_check returned: $INTEGRITY" >&2
  exit 1
fi
echo "ok: integrity_check"

# ── 3. Migration version check ──────────────────────────────────────────────
# The restored database must be at a real, fully-applied migration state:
# no failed migrations, and a max version matching the latest migration in
# the repo (the drill restores the latest backup, so they must agree).
# sqlx stores the migration version as the numeric prefix of the filename
# (0001_accounts.sql -> version 1), so compare as numbers.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXPECTED="$(ls "$REPO_ROOT"/migrations/*.sql 2>/dev/null \
  | sed 's/.*\///;s/_.*//' | sort -n | tail -1)"
# Strip leading zeros ("0011" -> 11) so the comparison is numeric, matching
# how sqlx records the version.
EXPECTED="$((10#$EXPECTED))"
LATEST="$(sqlite3 "$OUT" "SELECT MAX(version) FROM _sqlx_migrations;")"
FAILED="$(sqlite3 "$OUT" "SELECT COUNT(*) FROM _sqlx_migrations WHERE success = 0;")"
if [ "$FAILED" != "0" ]; then
  echo "FAIL: $FAILED migration(s) failed in the restored database" >&2
  exit 1
fi
if [ "$LATEST" != "$EXPECTED" ]; then
  if [ -n "$SNAPSHOT" ] && [ "$LATEST" -lt "$EXPECTED" ] 2>/dev/null; then
    # An older snapshot legitimately predates newer migrations; the backend
    # applies them on startup. Only a *newer* schema than the code is fatal.
    echo "warn: snapshot is at migration $LATEST, code expects $EXPECTED (will migrate on startup)"
  else
    echo "FAIL: restored migration version $LATEST != expected $EXPECTED" >&2
    exit 1
  fi
fi
echo "ok: migration version $LATEST"

# ── 4. Ledger reconciliation invariant ───────────────────────────────────────
# Every account's balance must equal its starting balance (100000) plus the
# sum of all cash flows from its positions: premium paid/received on open,
# and settlement received/paid on close. Any row returned here is a
# violation.
VIOLATIONS="$(sqlite3 "$OUT" "
SELECT a.wallet_address, a.balance AS actual, e.expected
  FROM accounts a
  JOIN (
        SELECT wallet_address,
               100000.0 + COALESCE(SUM(
                   (CASE WHEN position_type = 'short' THEN 1.0 ELSE -1.0 END)
                       * entry_premium * contracts
                   + CASE WHEN status IN ('closed', 'rolled')
                          THEN (CASE WHEN position_type = 'short' THEN -1.0 ELSE 1.0 END)
                               * close_premium * contracts
                          ELSE 0.0 END
               ), 0.0) AS expected
          FROM positions
         GROUP BY wallet_address
       ) e ON e.wallet_address = a.wallet_address
 WHERE ABS(a.balance - e.expected) > 0.01;")"
if [ -n "$VIOLATIONS" ]; then
  echo "FAIL: ledger reconciliation invariant violated:" >&2
  echo "$VIOLATIONS" >&2
  exit 1
fi
echo "ok: ledger reconciliation invariant"

# ── 5. Indexer state (informational) ────────────────────────────────────────
# The restored cursors sit at the backup point; the indexer must backfill
# from there to the chain tip before settlement outcomes are trusted again.
# This step reports what the restore brought back. The full gap/backfill
# plan is computed by dr::plan_indexer_recovery (see the DR runbook).
has_table() {
  [ "$(sqlite3 "$OUT" "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='$1';")" = "1" ]
}
if has_table indexer_cursors; then
  echo "indexer cursors (resume from ledger + 1):"
  sqlite3 -separator ' ' "$OUT" "SELECT '  ' || stream, ledger FROM indexer_cursors ORDER BY stream;"
else
  echo "info: no indexer_cursors table; indexer will start from its configured genesis"
fi
if has_table indexer_gap_alerts; then
  OPEN_GAPS="$(sqlite3 "$OUT" "SELECT COUNT(*) FROM indexer_gap_alerts WHERE resolved_at IS NULL;")"
  if [ "$OPEN_GAPS" != "0" ]; then
    echo "warn: $OPEN_GAPS unresolved indexer gap(s) in the restored database; backfill before go-live"
  fi
fi

# ── RTO ─────────────────────────────────────────────────────────────────────
END_NS=$(date +%s%N)
RTO_MS=$(( (END_NS - START_NS) / 1000000 ))
echo "RTO: ${RTO_MS}ms (restore + validation)"
echo "PASS: restore drill succeeded"
