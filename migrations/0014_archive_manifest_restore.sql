-- Archive manifests: record the archive format and when a batch was
-- restored back into the hot database, and index the lookups the retention
-- service and auditors make.
ALTER TABLE archive_manifests ADD COLUMN format TEXT NOT NULL DEFAULT 'jsonl-v1';
ALTER TABLE archive_manifests ADD COLUMN restored_at TEXT;

CREATE INDEX IF NOT EXISTS idx_archive_manifests_table_range
    ON archive_manifests (table_name, from_timestamp, to_timestamp);
