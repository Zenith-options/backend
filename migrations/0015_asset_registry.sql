-- Migration 0015: canonical asset registry
--
-- Every underlying and collateral asset is described once here. Settlement,
-- margin, collateral and price code paths resolve precision and contract
-- addresses through this table instead of hard-coding assumptions.
--
-- Rows are immutable-by-insert with an `enabled` flag for lifecycle: disabling
-- an asset never deletes historical rows, so old settlement records still
-- resolve the definition in force at the time.

CREATE TABLE IF NOT EXISTS asset_registry (
    -- Unique internal symbol used across the codebase (e.g. "XLM", "BTC").
    symbol              TEXT        NOT NULL,

    -- Stellar asset code (up to 12 chars, e.g. "XLM", "USDC").
    asset_code          TEXT        NOT NULL,

    -- Stellar issuer public key (G... strkey). NULL for the native asset (XLM).
    issuer              TEXT,

    -- Stellar Asset Contract address (C... strkey). NULL until SAC is deployed.
    sac_contract_id     TEXT,

    -- On-chain decimal precision. Stellar native/classic assets use 7.
    -- Bridged/wrapped assets may use more.
    decimals            INTEGER     NOT NULL DEFAULT 7
                            CHECK (decimals >= 0 AND decimals <= 18),

    -- Key used to look up the price in the oracle / spot-price map.
    price_source_key    TEXT        NOT NULL,

    -- How this asset may be used: comma-separated set of
    -- "underlying", "collateral", "settlement".
    supported_as        TEXT        NOT NULL DEFAULT 'underlying,collateral',

    -- Schema version for this row; bump when the interpretation changes.
    schema_version      INTEGER     NOT NULL DEFAULT 1,

    -- Soft lifecycle flag; disabled assets are not accepted for new positions
    -- but remain for historical resolution.
    enabled             INTEGER     NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),

    -- Audit timestamps.
    created_at          TEXT        NOT NULL
                            DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at          TEXT        NOT NULL
                            DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),

    -- (symbol, issuer) is globally unique; NULL issuer is the native asset.
    PRIMARY KEY (symbol, COALESCE(issuer, ''))
);

-- Fast lookups by code+issuer (ambiguous-code guard) and by SAC id.
CREATE INDEX IF NOT EXISTS idx_asset_registry_code_issuer
    ON asset_registry (asset_code, issuer);

CREATE INDEX IF NOT EXISTS idx_asset_registry_sac
    ON asset_registry (sac_contract_id)
    WHERE sac_contract_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_asset_registry_enabled
    ON asset_registry (enabled, symbol);

-- ── Testnet seed data ──────────────────────────────────────────────────────────
-- These are the four underlyings already seeded into AppState (src/lib.rs) plus
-- USDC as the settlement asset. Disable any row rather than deleting it.

INSERT OR IGNORE INTO asset_registry
    (symbol, asset_code, issuer, sac_contract_id, decimals, price_source_key, supported_as)
VALUES
    ('XLM',  'XLM',  NULL,
     NULL,
     7, 'XLM', 'underlying,collateral'),

    ('BTC',  'BTC',  'GAHJJJKMOKYE4RVPZEWZTKH5FVI4PA3VL7GK2LFNUBSGBV5GKIW7HI3',
     NULL,
     7, 'BTC', 'underlying'),

    ('ETH',  'ETH',  'GBFXOHVAS43OIWNIO7XLRJAHT3BICFEIKOJLZVXNT572MISM4CMGSOCC',
     NULL,
     7, 'ETH', 'underlying'),

    ('SOL',  'SOL',  'GBSTRH4QOPIBC7QOGZKWBU7XQLKC7MNTFGZ4VEBHXQBDIJYQF63TPCA',
     NULL,
     7, 'SOL', 'underlying'),

    ('USDC', 'USDC', 'GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5',
     NULL,
     7, 'USDC', 'collateral,settlement');
