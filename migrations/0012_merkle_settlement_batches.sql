-- 0012: Merkle-commitment batch settlement for expired option pools (#196).
--
-- One row per settlement batch, one row per payout leaf, and one row per
-- (possibly partial) claim. The batch root, leaf count, leaf encoding version
-- and total payout are all persisted at commit time and never change
-- afterwards, so a verifier can re-derive the commitment from the leaves and
-- compare. Amounts are stored in stroops (INTEGER, 7 dp), matching the
-- `Money`/`Qty` convention documented in `0001_core_schema.sql`.

CREATE TABLE settlement_batches (
    id                  TEXT PRIMARY KEY,
    payout_asset        TEXT NOT NULL,
    status              TEXT NOT NULL DEFAULT 'planned'
        CHECK (status IN ('planned', 'committed', 'finalized')),
    -- Committed only. NULL until the single signed commitment tx is accepted.
    root                TEXT,
    leaf_count          INTEGER,
    leaf_version        INTEGER NOT NULL DEFAULT 1,
    total_scaled        INTEGER NOT NULL,
    available_scaled    INTEGER NOT NULL,
    -- The one signed transaction carrying the commitment.
    commitment_tx_hash  TEXT,
    commitment_ledger   INTEGER,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    committed_at        TEXT,
    finalized_at        TEXT,
    -- A committed/finalized batch must carry the full commitment; a planned
    -- one must not.
    CHECK (
        (status = 'planned'
            AND root IS NULL AND leaf_count IS NULL AND committed_at IS NULL)
        OR
        (status IN ('committed', 'finalized')
            AND root IS NOT NULL AND leaf_count IS NOT NULL
            AND committed_at IS NOT NULL)
    ),
    CHECK (total_scaled <= available_scaled)
);

CREATE INDEX idx_settlement_batches_status ON settlement_batches(status);

-- One row per payout. `leaf_hash` is the canonical leaf-v1 hash (see
-- src/chain/merkle.rs); it is what the Merkle proof is built from. Leaves are
-- immutable once the batch is committed (enforced by the trigger below).
CREATE TABLE settlement_batch_leaves (
    batch_id        TEXT NOT NULL REFERENCES settlement_batches(id) ON DELETE CASCADE,
    position_id     TEXT NOT NULL,
    account         TEXT NOT NULL,
    amount_scaled   INTEGER NOT NULL CHECK (amount_scaled >= 0),
    claimed_scaled  INTEGER NOT NULL DEFAULT 0 CHECK (claimed_scaled >= 0),
    leaf_hash       TEXT NOT NULL,
    claimed_at      TEXT,
    PRIMARY KEY (batch_id, position_id, account),
    -- A leaf can never be claimed beyond its committed amount.
    CHECK (claimed_scaled <= amount_scaled)
);

CREATE INDEX idx_settlement_leaves_account
    ON settlement_batch_leaves(account, batch_id);

-- Idempotency + audit trail for claims. Replaying the same
-- (batch_id, position_id, idempotency_key) is a no-op that returns the
-- original row; the unique index is the guard.
CREATE TABLE settlement_claims (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    batch_id            TEXT NOT NULL REFERENCES settlement_batches(id) ON DELETE CASCADE,
    position_id         TEXT NOT NULL,
    account             TEXT NOT NULL,
    amount_scaled       INTEGER NOT NULL CHECK (amount_scaled > 0),
    idempotency_key     TEXT NOT NULL,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (batch_id, position_id, idempotency_key)
);

CREATE INDEX idx_settlement_claims_batch ON settlement_claims(batch_id);

-- ─── Integrity triggers ──────────────────────────────────────────────────────
-- The leaf set is frozen the moment a batch is committed: no leaf may be
-- inserted, updated in a way that changes the payout, or deleted. Partial
-- claims only ever update `claimed_scaled`/`claimed_at`, which is allowed as
-- long as the running total stays within the committed amount (CHECK above).

CREATE TRIGGER trg_settlement_leaves_locked_insert
BEFORE INSERT ON settlement_batch_leaves
FOR EACH ROW
WHEN (SELECT status FROM settlement_batches WHERE id = NEW.batch_id)
     IN ('committed', 'finalized')
BEGIN
    SELECT RAISE(ABORT, 'leaf set is locked after commitment');
END;

CREATE TRIGGER trg_settlement_leaves_locked_update
BEFORE UPDATE OF position_id, account, amount_scaled, leaf_hash
ON settlement_batch_leaves
FOR EACH ROW
WHEN (SELECT status FROM settlement_batches WHERE id = NEW.batch_id)
     IN ('committed', 'finalized')
BEGIN
    SELECT RAISE(ABORT, 'leaf set is locked after commitment');
END;

CREATE TRIGGER trg_settlement_leaves_locked_delete
BEFORE DELETE ON settlement_batch_leaves
FOR EACH ROW
WHEN (SELECT status FROM settlement_batches WHERE id = OLD.batch_id)
     IN ('committed', 'finalized')
BEGIN
    SELECT RAISE(ABORT, 'leaf set is locked after commitment');
END;
