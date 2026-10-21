-- Insurance reserve and liquidation deficit ledger.
--
-- A liquidation that can't recover an account's full shortfall (fast or
-- illiquid markets) must not quietly become an unsettled liability. Every
-- shortfall is recorded here as a deficit, covered from the reserve as far
-- as the reserve allows, and anything left stays visibly open until it is
-- covered by later contributions or explicitly written off.
--
-- All amounts are INTEGER stroops (1e-7) of the settlement asset, so sums
-- are exact.

-- Current reserve balance. A single row so every change is one UPDATE that
-- takes SQLite's write lock before reading the balance it changes.
CREATE TABLE insurance_reserve_state (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    balance     INTEGER NOT NULL DEFAULT 0 CHECK (balance >= 0),
    updated_at  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
INSERT INTO insurance_reserve_state (id, balance) VALUES (1, 0);

-- Append-only journal of every reserve movement. SUM(amount) must always
-- equal insurance_reserve_state.balance.
CREATE TABLE insurance_reserve_entries (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    kind          TEXT    NOT NULL CHECK (kind IN (
                      'contribution',        -- treasury / governance top-up
                      'liquidation_penalty', -- penalty share from a liquidation
                      'fee_share',           -- share of protocol fees
                      'deficit_cover',       -- draw to cover a deficit
                      'withdrawal'           -- governance-approved withdrawal
                  )),
    amount        INTEGER NOT NULL CHECK (amount <> 0),
    balance_after INTEGER NOT NULL CHECK (balance_after >= 0),
    deficit_id    TEXT    REFERENCES liquidation_deficits (id),
    reference     TEXT,
    memo          TEXT,
    created_at    TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    -- Inflows are positive, outflows negative.
    CHECK ((kind IN ('deficit_cover', 'withdrawal')) = (amount < 0)),
    -- A cover draw always names the deficit it covers.
    CHECK ((kind = 'deficit_cover') = (deficit_id IS NOT NULL))
);
CREATE INDEX idx_reserve_entries_deficit ON insurance_reserve_entries (deficit_id)
    WHERE deficit_id IS NOT NULL;
CREATE INDEX idx_reserve_entries_created ON insurance_reserve_entries (created_at);

CREATE TRIGGER insurance_reserve_entries_no_update
BEFORE UPDATE ON insurance_reserve_entries
BEGIN
    SELECT RAISE(ABORT, 'insurance_reserve_entries is append-only');
END;

CREATE TRIGGER insurance_reserve_entries_no_delete
BEFORE DELETE ON insurance_reserve_entries
BEGIN
    SELECT RAISE(ABORT, 'insurance_reserve_entries is append-only');
END;

-- One row per liquidation that ended with a shortfall.
-- outstanding = shortfall - covered - written_off.
CREATE TABLE liquidation_deficits (
    id              TEXT    PRIMARY KEY,
    -- The liquidation that produced the shortfall. Unique so recording the
    -- same liquidation twice can never draw from the reserve twice.
    liquidation_ref TEXT    NOT NULL UNIQUE,
    account         TEXT    NOT NULL,
    underlying      TEXT,
    shortfall       INTEGER NOT NULL CHECK (shortfall > 0),
    covered         INTEGER NOT NULL DEFAULT 0 CHECK (covered >= 0),
    written_off     INTEGER NOT NULL DEFAULT 0 CHECK (written_off >= 0),
    status          TEXT    NOT NULL DEFAULT 'open'
                        CHECK (status IN ('open', 'covered', 'written_off')),
    write_off_memo  TEXT,
    created_at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    resolved_at     TEXT,
    CHECK (covered + written_off <= shortfall),
    CHECK ((status = 'open') = (covered + written_off < shortfall))
);
CREATE INDEX idx_liquidation_deficits_open ON liquidation_deficits (created_at)
    WHERE status = 'open';
CREATE INDEX idx_liquidation_deficits_account ON liquidation_deficits (account);
