-- Migration 0013: one-shot cutover from off-chain balances to on-chain escrow.
--
-- The cutover is rehearsed, resumable and reversible (see src/cutover.rs and
-- docs/runbooks/onchain-cutover.md). Nothing here deletes or rewrites the
-- existing accounts/positions rows. They remain the off-chain record, and a
-- rollback simply makes them authoritative again.
--
--   * cutover_runs              — one row per cutover attempt.
--   * cutover_snapshot_wallets  — the frozen pre-cutover state each run is
--                                 reconciled against. Immutable.
--   * cutover_wallets           — per-wallet progress, including exactly how
--                                 much was credited on-chain (for resume and
--                                 rollback).
--   * account_settlement_mode   — where a wallet's funds live: offchain,
--                                 migrating (frozen) or onchain.
--   * cutover_events            — append-only audit log.
--
-- Amounts are INTEGER stroops (10^-7). Timestamps are Unix epoch seconds.

CREATE TABLE IF NOT EXISTS cutover_runs (
    id                TEXT    PRIMARY KEY,
    label             TEXT    NOT NULL,
    status            TEXT    NOT NULL CHECK (status IN
                          ('snapshotted', 'migrating', 'migrated', 'verified', 'rolled_back')),
    contract_id       TEXT    NOT NULL,
    snapshot_sha256   TEXT    NOT NULL,
    wallet_count      INTEGER NOT NULL,
    total_free        INTEGER NOT NULL,
    total_collateral  INTEGER NOT NULL,
    created_by        TEXT    NOT NULL,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL
);

-- At most one cutover in flight at a time.
CREATE UNIQUE INDEX IF NOT EXISTS cutover_runs_one_active
    ON cutover_runs ((1))
    WHERE status IN ('snapshotted', 'migrating', 'migrated');

CREATE TABLE IF NOT EXISTS cutover_snapshot_wallets (
    run_id                  TEXT    NOT NULL REFERENCES cutover_runs (id),
    wallet_address          TEXT    NOT NULL,
    free_stroops            INTEGER NOT NULL,
    collateral_stroops      INTEGER NOT NULL,
    open_positions          INTEGER NOT NULL,
    open_collateral_stroops INTEGER NOT NULL,
    -- SHA-256 over the wallet's open positions, for drift detection.
    positions_sha256        TEXT    NOT NULL,
    PRIMARY KEY (run_id, wallet_address)
);

CREATE TRIGGER IF NOT EXISTS cutover_snapshot_wallets_no_update
    BEFORE UPDATE ON cutover_snapshot_wallets
BEGIN
    SELECT RAISE(ABORT, 'cutover snapshots are immutable');
END;

CREATE TRIGGER IF NOT EXISTS cutover_snapshot_wallets_no_delete
    BEFORE DELETE ON cutover_snapshot_wallets
BEGIN
    SELECT RAISE(ABORT, 'cutover snapshots are immutable');
END;

CREATE TABLE IF NOT EXISTS cutover_wallets (
    run_id               TEXT    NOT NULL REFERENCES cutover_runs (id),
    wallet_address       TEXT    NOT NULL,
    status               TEXT    NOT NULL CHECK (status IN
                             ('pending', 'migrated', 'held', 'failed', 'reverted')),
    free_credited        INTEGER NOT NULL DEFAULT 0,
    collateral_credited  INTEGER NOT NULL DEFAULT 0,
    free_tx              TEXT,
    collateral_tx        TEXT,
    detail               TEXT,
    updated_at           INTEGER NOT NULL,
    PRIMARY KEY (run_id, wallet_address)
);

CREATE INDEX IF NOT EXISTS idx_cutover_wallets_status
    ON cutover_wallets (run_id, status);

CREATE TABLE IF NOT EXISTS account_settlement_mode (
    wallet_address  TEXT    PRIMARY KEY,
    mode            TEXT    NOT NULL CHECK (mode IN ('offchain', 'migrating', 'onchain')),
    run_id          TEXT    REFERENCES cutover_runs (id),
    updated_at      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS cutover_events (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id          TEXT    NOT NULL REFERENCES cutover_runs (id),
    wallet_address  TEXT,
    event           TEXT    NOT NULL,
    actor           TEXT    NOT NULL,
    detail          TEXT    NOT NULL DEFAULT '',
    created_at      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_cutover_events_run
    ON cutover_events (run_id, id);

CREATE TRIGGER IF NOT EXISTS cutover_events_no_update
    BEFORE UPDATE ON cutover_events
BEGIN
    SELECT RAISE(ABORT, 'cutover_events is append-only');
END;

CREATE TRIGGER IF NOT EXISTS cutover_events_no_delete
    BEFORE DELETE ON cutover_events
BEGIN
    SELECT RAISE(ABORT, 'cutover_events is append-only');
END;
