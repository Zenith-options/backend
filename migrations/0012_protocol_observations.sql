-- Every Soroban protocol compatibility check (see src/soroban/protocol.rs).
-- Gives operators a history of what protocol the network / RPC reported and
-- what the gate decided, e.g. to confirm exactly when an upgrade landed.
CREATE TABLE IF NOT EXISTS protocol_observations (
    id               TEXT    PRIMARY KEY,
    observed_at      INTEGER NOT NULL,
    rpc_protocol     INTEGER NOT NULL,
    ledger_protocol  INTEGER NOT NULL,
    latest_ledger    INTEGER NOT NULL,
    passphrase       TEXT    NOT NULL,
    rpc_version      TEXT,
    core_version     TEXT,
    sdk_version      TEXT    NOT NULL,
    sdk_min_protocol INTEGER NOT NULL,
    sdk_max_protocol INTEGER NOT NULL,
    status           TEXT    NOT NULL
        CHECK (status IN ('compatible', 'deploy_blocked', 'incompatible')),
    findings         TEXT    NOT NULL DEFAULT '[]'
);

CREATE INDEX IF NOT EXISTS protocol_observations_observed_at_idx
    ON protocol_observations (observed_at);
