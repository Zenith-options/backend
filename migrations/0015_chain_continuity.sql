-- Chain-state continuity tracking (see src/soroban/continuity.rs).

-- Singleton: which network the lineage belongs to, and how many times it
-- has been reset (each reset starts a new epoch).
CREATE TABLE IF NOT EXISTS chain_continuity_state (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    epoch      INTEGER NOT NULL DEFAULT 0,
    passphrase TEXT    NOT NULL
);

-- The canonical ledger line as observed. Entries above a fork point are
-- deleted (and logged in chain_discontinuities) when it is discarded.
CREATE TABLE IF NOT EXISTS ledger_lineage (
    sequence    INTEGER PRIMARY KEY,
    hash        TEXT    NOT NULL,
    prev_hash   TEXT,
    close_time  INTEGER NOT NULL,
    epoch       INTEGER NOT NULL,
    observed_at TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE IF NOT EXISTS chain_discontinuities (
    id                TEXT    PRIMARY KEY,
    kind              TEXT    NOT NULL
        CHECK (kind IN ('sequence_regression', 'hash_mismatch', 'aborted_close', 'network_reset')),
    fork_sequence     INTEGER,
    observed_sequence INTEGER NOT NULL,
    observed_hash     TEXT    NOT NULL,
    expected_hash     TEXT,
    discarded         TEXT    NOT NULL DEFAULT '[]',
    epoch_before      INTEGER NOT NULL,
    epoch_after       INTEGER NOT NULL,
    reverted_records  INTEGER NOT NULL DEFAULT 0,
    detected_at       INTEGER NOT NULL
);

-- Off-chain state derived from on-chain transactions. Rows built on a
-- discarded ledger line move active -> reverted, then to rederived (with a
-- link to the replacement row) or orphaned; they are never deleted.
CREATE TABLE IF NOT EXISTS chain_derived_records (
    id              TEXT    PRIMARY KEY,
    kind            TEXT    NOT NULL,
    ref_id          TEXT    NOT NULL,
    tx_hash         TEXT    NOT NULL,
    ledger_sequence INTEGER NOT NULL,
    ledger_hash     TEXT    NOT NULL,
    epoch           INTEGER NOT NULL,
    status          TEXT    NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'reverted', 'rederived', 'orphaned')),
    reverted_by     TEXT    REFERENCES chain_discontinuities (id),
    revert_reason   TEXT,
    rederived_as    TEXT    REFERENCES chain_derived_records (id),
    created_at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS chain_derived_records_status_ledger_idx
    ON chain_derived_records (status, ledger_sequence);
CREATE INDEX IF NOT EXISTS chain_derived_records_tx_hash_idx
    ON chain_derived_records (tx_hash);
