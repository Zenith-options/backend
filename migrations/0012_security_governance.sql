CREATE TABLE IF NOT EXISTS audit_log (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    ts INTEGER NOT NULL,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    subject TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    prev_hash TEXT NOT NULL,
    hash TEXT NOT NULL UNIQUE
);

CREATE TRIGGER IF NOT EXISTS audit_log_no_update BEFORE UPDATE ON audit_log
BEGIN SELECT RAISE(ABORT, 'audit_log is append-only'); END;
CREATE TRIGGER IF NOT EXISTS audit_log_no_delete BEFORE DELETE ON audit_log
BEGIN SELECT RAISE(ABORT, 'audit_log is append-only'); END;

CREATE TABLE IF NOT EXISTS abuse_cases (
    id TEXT PRIMARY KEY,
    rule TEXT NOT NULL,
    severity TEXT NOT NULL,
    evidence_json TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'open',
    assignee TEXT,
    restriction TEXT,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS gov_proposals (id TEXT PRIMARY KEY, state TEXT NOT NULL, starts_at INTEGER NOT NULL, ends_at INTEGER NOT NULL, cursor TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS gov_votes (proposal_id TEXT NOT NULL, voter TEXT NOT NULL, choice TEXT NOT NULL, power TEXT NOT NULL, ledger INTEGER NOT NULL, PRIMARY KEY (proposal_id, voter));
CREATE TABLE IF NOT EXISTS gov_delegations (delegator TEXT PRIMARY KEY, delegate TEXT NOT NULL, ledger INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS gov_actions (proposal_id TEXT NOT NULL, action_index INTEGER NOT NULL, action_json TEXT NOT NULL, PRIMARY KEY (proposal_id, action_index));
