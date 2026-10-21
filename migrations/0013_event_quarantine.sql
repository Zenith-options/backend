-- Settlement events the indexer could not decode (unknown event, unknown
-- schema version, or a body that violates its declared schema). They are
-- kept here with the full raw payload instead of being dropped, and replayed
-- once a decoder for their version ships. See src/soroban/events.rs.
CREATE TABLE IF NOT EXISTS event_quarantine (
    event_id        TEXT    PRIMARY KEY,
    contract_id     TEXT    NOT NULL,
    ledger          INTEGER NOT NULL,
    tx_hash         TEXT    NOT NULL,
    event_name      TEXT,
    schema_version  INTEGER,
    reason          TEXT    NOT NULL
        CHECK (reason IN ('malformed_envelope', 'unknown_event', 'unknown_version', 'schema_violation')),
    detail          TEXT    NOT NULL,
    topics          TEXT    NOT NULL,
    data            TEXT    NOT NULL,
    status          TEXT    NOT NULL DEFAULT 'quarantined'
        CHECK (status IN ('quarantined', 'released', 'ignored')),
    attempts        INTEGER NOT NULL DEFAULT 1,
    first_seen_at   TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    last_attempt_at TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    released_at     TEXT
);

CREATE INDEX IF NOT EXISTS event_quarantine_status_ledger_idx
    ON event_quarantine (status, ledger);
