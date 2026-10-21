-- Auditable record of automated exercise decisions and deterministic writer
-- assignments. Contract settlement remains authoritative for final balances.
CREATE TABLE expiry_settlement_runs (
    id                  TEXT PRIMARY KEY,
    series_id           TEXT NOT NULL,
    underlying          TEXT NOT NULL,
    expires_at          TEXT NOT NULL,
    fixing_price        REAL NOT NULL CHECK (fixing_price >= 0),
    minimum_intrinsic   REAL NOT NULL CHECK (minimum_intrinsic >= 0),
    chain_tx_hash       TEXT NOT NULL UNIQUE,
    recorded_at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE expiry_exercise_decisions (
    run_id              TEXT NOT NULL REFERENCES expiry_settlement_runs(id),
    long_position_id    TEXT NOT NULL REFERENCES positions(id),
    exercised_contracts INTEGER NOT NULL CHECK (exercised_contracts > 0),
    intrinsic_per_contract REAL NOT NULL CHECK (intrinsic_per_contract > 0),
    PRIMARY KEY (run_id, long_position_id)
);

CREATE TABLE expiry_writer_assignments (
    run_id              TEXT NOT NULL REFERENCES expiry_settlement_runs(id),
    short_position_id   TEXT NOT NULL REFERENCES positions(id),
    assigned_contracts INTEGER NOT NULL CHECK (assigned_contracts > 0),
    PRIMARY KEY (run_id, short_position_id)
);

CREATE INDEX expiry_exercise_decisions_position
    ON expiry_exercise_decisions(long_position_id);
CREATE INDEX expiry_writer_assignments_position
    ON expiry_writer_assignments(short_position_id);
