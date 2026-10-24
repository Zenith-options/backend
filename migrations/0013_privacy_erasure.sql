-- Minimal audit record for requests to erase off-chain account data.
-- The wallet remains as a pseudonymous financial/on-chain audit anchor.
CREATE TABLE IF NOT EXISTS privacy_erasure_log (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    wallet_anchor   TEXT NOT NULL,
    erased_at       TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_privacy_erasure_wallet
    ON privacy_erasure_log(wallet_anchor, erased_at);
