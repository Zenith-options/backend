-- Migration 0014: settlement batch previews and the human approval queue.
--
-- Settlement batches move collateral unattended. Every batch is now
-- proposed with an exact preview (per-leg and per-wallet money movements,
-- hashed). It either auto-approves under policy limits or waits for a
-- human, and executes only if a fresh recomputation still produces the
-- same hash. See src/settlement_review.rs and docs/settlement-review.md.
--
-- Amounts are INTEGER stroops (10^-7). Timestamps are Unix epoch seconds.

CREATE TABLE IF NOT EXISTS settlement_batches (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    underlying           TEXT    NOT NULL,
    expires_at           TEXT    NOT NULL,
    fixing_price         INTEGER NOT NULL,
    fixing_source        TEXT    NOT NULL CHECK (fixing_source IN ('twap', 'fallback', 'manual')),
    preview_json         TEXT    NOT NULL,
    preview_sha256       TEXT    NOT NULL,
    gross_value          INTEGER NOT NULL,
    positions            INTEGER NOT NULL,
    status               TEXT    NOT NULL CHECK (status IN (
                             'pending_review', 'approved', 'auto_approved', 'rejected',
                             'executing', 'executed', 'failed', 'stale', 'expired',
                             'compliance_hold')),
    -- JSON array: why the policy required review (empty when auto-approved).
    review_reasons       TEXT    NOT NULL DEFAULT '[]',
    proposed_by          TEXT    NOT NULL,
    proposed_at          INTEGER NOT NULL,
    decided_by           TEXT,
    decided_at           INTEGER,
    decision_note        TEXT,
    approval_expires_at  INTEGER,
    executed_by          TEXT,
    executed_at          INTEGER,
    execution_ref        TEXT,
    updated_at           INTEGER NOT NULL
);

-- One live batch per expiry: a second proposal must supersede the first.
CREATE UNIQUE INDEX IF NOT EXISTS settlement_batches_one_live
    ON settlement_batches (underlying, expires_at)
    WHERE status IN ('pending_review', 'approved', 'auto_approved', 'executing', 'failed');

CREATE INDEX IF NOT EXISTS idx_settlement_batches_status
    ON settlement_batches (status, proposed_at);

-- What was reviewed can never change underneath the reviewer.
CREATE TRIGGER IF NOT EXISTS settlement_batches_preview_immutable
    BEFORE UPDATE OF preview_json, preview_sha256, fixing_price, fixing_source, underlying, expires_at
    ON settlement_batches
BEGIN
    SELECT RAISE(ABORT, 'settlement batch preview is immutable');
END;

-- Terminal states are final.
CREATE TRIGGER IF NOT EXISTS settlement_batches_terminal
    BEFORE UPDATE OF status ON settlement_batches
    WHEN OLD.status IN ('executed', 'rejected', 'stale', 'expired', 'compliance_hold')
BEGIN
    SELECT RAISE(ABORT, 'settlement batch is in a terminal state');
END;

CREATE TRIGGER IF NOT EXISTS settlement_batches_no_delete
    BEFORE DELETE ON settlement_batches
BEGIN
    SELECT RAISE(ABORT, 'settlement batches cannot be deleted');
END;

CREATE TABLE IF NOT EXISTS settlement_batch_events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    batch_id    INTEGER NOT NULL REFERENCES settlement_batches (id),
    event       TEXT    NOT NULL,
    actor       TEXT    NOT NULL,
    detail      TEXT    NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_settlement_batch_events_batch
    ON settlement_batch_events (batch_id, id);

CREATE TRIGGER IF NOT EXISTS settlement_batch_events_no_update
    BEFORE UPDATE ON settlement_batch_events
BEGIN
    SELECT RAISE(ABORT, 'settlement_batch_events is append-only');
END;

CREATE TRIGGER IF NOT EXISTS settlement_batch_events_no_delete
    BEFORE DELETE ON settlement_batch_events
BEGIN
    SELECT RAISE(ABORT, 'settlement_batch_events is append-only');
END;
