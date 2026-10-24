-- Durable, bounded settlement job queue with priority classes.
-- See src/settlement_queue.rs for the scheduling and load-shedding rules.
CREATE TABLE settlement_jobs (
    seq              INTEGER PRIMARY KEY AUTOINCREMENT,
    id               TEXT NOT NULL UNIQUE,
    kind             TEXT NOT NULL
                     CHECK (kind IN ('liquidation', 'expiry', 'settlement', 'backfill', 'oracle_republish')),
    class            TEXT NOT NULL CHECK (class IN ('urgent', 'normal', 'bulk')),
    payload          TEXT NOT NULL DEFAULT '{}',
    dedupe_key       TEXT,
    status           TEXT NOT NULL DEFAULT 'queued'
                     CHECK (status IN ('queued', 'leased', 'done', 'dead')),
    attempts         INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    max_attempts     INTEGER NOT NULL DEFAULT 5 CHECK (max_attempts > 0),
    available_at_ms  INTEGER NOT NULL,
    lease_owner      TEXT,
    lease_expires_ms INTEGER,
    last_error       TEXT,
    created_at_ms    INTEGER NOT NULL,
    updated_at_ms    INTEGER NOT NULL
);

-- Claim path: oldest ready job within a class.
CREATE INDEX idx_settlement_jobs_ready ON settlement_jobs(class, status, available_at_ms, seq);
-- Lease reaper.
CREATE INDEX idx_settlement_jobs_lease ON settlement_jobs(status, lease_expires_ms);
-- A logical piece of work may only be active once at a time.
CREATE UNIQUE INDEX idx_settlement_jobs_active_dedupe
    ON settlement_jobs(dedupe_key)
    WHERE dedupe_key IS NOT NULL AND status IN ('queued', 'leased');
