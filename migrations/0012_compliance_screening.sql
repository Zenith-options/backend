-- Migration 0012: sanctions / AML screening for value-moving flows.
--
-- Every wallet that deposits, withdraws, opens a position or receives a
-- settlement payout is screened before funds move (see src/compliance.rs
-- and docs/compliance.md). Three tables:
--
--   * compliance_screenings — raw provider results, reused as a cache
--     until they age past the configured TTL.
--   * compliance_overrides  — operator-issued allow/deny overrides, with
--     a mandatory reason and an optional expiry.
--   * compliance_decisions  — one row per gate decision. Append-only: the
--     triggers below reject UPDATE and DELETE so the audit trail cannot be
--     rewritten after the fact.
--
-- Timestamps are Unix epoch seconds (INTEGER) so TTL and expiry checks are
-- plain integer comparisons.

CREATE TABLE IF NOT EXISTS compliance_screenings (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    wallet_address  TEXT    NOT NULL,
    provider        TEXT    NOT NULL,
    verdict         TEXT    NOT NULL CHECK (verdict IN ('clear', 'review', 'blocked')),
    risk_score      INTEGER NOT NULL CHECK (risk_score BETWEEN 0 AND 100),
    -- JSON array of provider-supplied reasons (list names, typologies, ...).
    reasons         TEXT    NOT NULL DEFAULT '[]',
    list_version    TEXT,
    screened_at     INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_compliance_screenings_wallet
    ON compliance_screenings (wallet_address, screened_at DESC);

CREATE TABLE IF NOT EXISTS compliance_overrides (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    wallet_address  TEXT    NOT NULL,
    action          TEXT    NOT NULL CHECK (action IN ('allow', 'deny')),
    reason          TEXT    NOT NULL CHECK (length(trim(reason)) > 0),
    operator        TEXT    NOT NULL CHECK (length(trim(operator)) > 0),
    created_at      INTEGER NOT NULL,
    expires_at      INTEGER,
    revoked_at      INTEGER,
    revoked_by      TEXT
);

CREATE INDEX IF NOT EXISTS idx_compliance_overrides_wallet
    ON compliance_overrides (wallet_address, created_at DESC);

CREATE TABLE IF NOT EXISTS compliance_decisions (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    wallet_address  TEXT    NOT NULL,
    flow            TEXT    NOT NULL CHECK (flow IN ('deposit', 'withdrawal', 'open_position', 'settlement')),
    outcome         TEXT    NOT NULL CHECK (outcome IN ('allow', 'deny')),
    -- 0 when the gate ran in monitor mode and would-have-denied is recorded
    -- but not enforced.
    enforced        INTEGER NOT NULL CHECK (enforced IN (0, 1)),
    source          TEXT    NOT NULL CHECK (source IN ('screening', 'cache', 'override', 'unavailable')),
    verdict         TEXT    CHECK (verdict IS NULL OR verdict IN ('clear', 'review', 'blocked')),
    reason          TEXT    NOT NULL,
    screening_id    INTEGER REFERENCES compliance_screenings (id),
    override_id     INTEGER REFERENCES compliance_overrides (id),
    created_at      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_compliance_decisions_wallet
    ON compliance_decisions (wallet_address, created_at DESC);

CREATE TRIGGER IF NOT EXISTS compliance_decisions_no_update
    BEFORE UPDATE ON compliance_decisions
BEGIN
    SELECT RAISE(ABORT, 'compliance_decisions is append-only');
END;

CREATE TRIGGER IF NOT EXISTS compliance_decisions_no_delete
    BEFORE DELETE ON compliance_decisions
BEGIN
    SELECT RAISE(ABORT, 'compliance_decisions is append-only');
END;
