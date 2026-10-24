-- Decoded diagnostics for failed Soroban invocations, one row per failed
-- simulate / submit / confirm of a transaction attempt. `record` holds the
-- full ForensicsRecord JSON (call trace, error events, logs, raw XDR); the
-- other columns exist for filtering. See src/soroban/forensics.rs.
CREATE TABLE IF NOT EXISTS tx_forensics (
    id                  TEXT    PRIMARY KEY,
    attempt_id          TEXT    NOT NULL,
    tx_hash             TEXT,
    stage               TEXT    NOT NULL CHECK (stage IN ('simulate', 'submit', 'confirm')),
    contract_id         TEXT,
    function            TEXT,
    rpc_status          TEXT,
    ledger              INTEGER,
    tx_result           TEXT,
    op_result           TEXT,
    host_error_kind     TEXT,
    host_error_code     TEXT,
    contract_error_code INTEGER,
    summary             TEXT    NOT NULL,
    record              TEXT    NOT NULL,
    captured_at         INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS tx_forensics_attempt_idx ON tx_forensics (attempt_id);
CREATE INDEX IF NOT EXISTS tx_forensics_tx_hash_idx ON tx_forensics (tx_hash);
CREATE INDEX IF NOT EXISTS tx_forensics_captured_at_idx ON tx_forensics (captured_at);
