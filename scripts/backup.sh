#!/usr/bin/env bash
#
# Take a consistent, self-verifying snapshot of the Zenith SQLite database.
#
# This is the second, independent backup layer next to Litestream's
# continuous WAL replication (see docs/runbooks/disaster-recovery.md).
# Litestream gives a small RPO; snapshots protect against losing or
# corrupting the Litestream replica itself, and give a single file that can
# be restored with nothing but sqlite3.
#
# Usage:
#   scripts/backup.sh [db-path] [out-dir]
#
#   [db-path]  database to snapshot (default: /data/zenith.db)
#   [out-dir]  directory for snapshots (default: /data/backups)
#
# Environment:
#   BACKUP_KEEP     number of local snapshots to keep (default: 14)
#   BACKUP_S3_URI   if set (e.g. s3://zenith-snapshots/prod), upload the
#                   snapshot and manifest there with `aws s3 cp`. Use a
#                   different bucket/account from the Litestream replica so
#                   one compromised credential can't delete both.
#
# Output, per run:
#   zenith-<UTC timestamp>.db.gz            gzip of an online .backup copy
#   zenith-<UTC timestamp>.manifest.json    sha256, size, migration version,
#                                           indexer cursors, row counts
#
# Requires: sqlite3, gzip, sha256sum (or shasum).

set -euo pipefail

DB="${1:-/data/zenith.db}"
OUT_DIR="${2:-/data/backups}"
KEEP="${BACKUP_KEEP:-14}"

if [ ! -f "$DB" ]; then
  echo "ERROR: database not found at $DB" >&2
  exit 1
fi
mkdir -p "$OUT_DIR"

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

table_exists() {
  [ "$(sqlite3 "$1" "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='$2';")" = "1" ]
}

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
BASE="$OUT_DIR/zenith-$STAMP"
TMP="$BASE.db.partial"
trap 'rm -f "$TMP"' EXIT

# ── 1. Online, transactionally consistent copy ──────────────────────────────
# `.backup` uses SQLite's backup API: it copies a consistent snapshot while
# the backend keeps writing (WAL mode), unlike `cp`, which can tear pages.
sqlite3 "$DB" ".timeout 10000" ".backup '$TMP'"

# ── 2. Verify the copy before keeping it ────────────────────────────────────
INTEGRITY="$(sqlite3 "$TMP" "PRAGMA integrity_check;")"
if [ "$INTEGRITY" != "ok" ]; then
  echo "FAIL: snapshot integrity_check returned: $INTEGRITY" >&2
  exit 1
fi

MIGRATION="null"
if table_exists "$TMP" _sqlx_migrations; then
  MIGRATION="$(sqlite3 "$TMP" "SELECT COALESCE(MAX(version), 'null') FROM _sqlx_migrations WHERE success = 1;")"
fi

CURSORS="[]"
if table_exists "$TMP" indexer_cursors; then
  CURSORS="$(sqlite3 "$TMP" "SELECT COALESCE(json_group_array(json_object('stream', stream, 'ledger', ledger)), '[]') FROM indexer_cursors;")"
fi

# Row counts for the tables that carry settlement evidence, so a restore can
# be sanity-checked against the manifest without trusting the file blindly.
COUNTS="{}"
for t in accounts positions settlement_fixings chain_events liquidation_deficits insurance_reserve_entries; do
  if table_exists "$TMP" "$t"; then
    n="$(sqlite3 "$TMP" "SELECT COUNT(*) FROM \"$t\";")"
    COUNTS="$(sqlite3 :memory: "SELECT json_set('$COUNTS', '\$.$t', $n);")"
  fi
done

# ── 3. Compress, checksum, write manifest ───────────────────────────────────
gzip -c "$TMP" > "$BASE.db.gz"
rm -f "$TMP"
SIZE="$(wc -c < "$BASE.db.gz" | tr -d ' ')"
SUM="$(sha256 "$BASE.db.gz")"

cat > "$BASE.manifest.json" <<EOF
{
  "file": "$(basename "$BASE.db.gz")",
  "sha256": "$SUM",
  "bytes": $SIZE,
  "taken_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "source": "$DB",
  "migration_version": $MIGRATION,
  "indexer_cursors": $CURSORS,
  "row_counts": $COUNTS
}
EOF
echo "snapshot: $BASE.db.gz ($SIZE bytes, sha256 $SUM)"

# ── 4. Optional off-site copy ───────────────────────────────────────────────
if [ -n "${BACKUP_S3_URI:-}" ]; then
  aws s3 cp "$BASE.db.gz" "$BACKUP_S3_URI/$(basename "$BASE.db.gz")"
  aws s3 cp "$BASE.manifest.json" "$BACKUP_S3_URI/$(basename "$BASE.manifest.json")"
  echo "uploaded to $BACKUP_S3_URI"
fi

# ── 5. Local rotation ───────────────────────────────────────────────────────
# Lexicographic order == chronological order for the UTC stamp.
ls -1 "$OUT_DIR"/zenith-*.db.gz 2>/dev/null | sort | head -n "-$KEEP" | while read -r old; do
  rm -f "$old" "${old%.db.gz}.manifest.json"
done

echo "PASS: backup complete"
