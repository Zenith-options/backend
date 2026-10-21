-- Shadow-mode settlement comparisons.
--
-- In shadow mode the off-chain engine stays authoritative and every margin,
-- payoff and expiry decision is also evaluated on the on-chain path by
-- simulation only (nothing is submitted). Each pair of results is recorded
-- here. After cutover, live mode keeps recording so a contract upgrade that
-- changes behaviour shows up as divergences tagged with its version.
--
-- Amounts are INTEGER stroops (1e-7).

CREATE TABLE shadow_comparisons (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    mode                TEXT    NOT NULL CHECK (mode IN ('shadow', 'live')),
    kind                TEXT    NOT NULL CHECK (kind IN ('margin', 'payoff', 'expiry')),
    subject_id          TEXT    NOT NULL,
    -- Contract wasm hash / version that produced the on-chain result.
    contract_version    TEXT    NOT NULL,
    off_chain_amount    INTEGER NOT NULL,
    off_chain_exercised INTEGER CHECK (off_chain_exercised IN (0, 1)),
    on_chain_amount     INTEGER,
    on_chain_exercised  INTEGER CHECK (on_chain_exercised IN (0, 1)),
    -- on_chain_amount - off_chain_amount
    diff                INTEGER,
    verdict             TEXT    NOT NULL
                            CHECK (verdict IN ('match', 'within_tolerance', 'diverged', 'on_chain_error')),
    detail              TEXT,
    -- JSON inputs the decision was made from, for replaying a divergence.
    inputs              TEXT,
    observed_at         TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX idx_shadow_comparisons_window
    ON shadow_comparisons (contract_version, kind, observed_at);
CREATE INDEX idx_shadow_comparisons_divergent
    ON shadow_comparisons (observed_at)
    WHERE verdict IN ('diverged', 'on_chain_error');
