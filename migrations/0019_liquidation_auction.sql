-- Sealed-bid liquidation auction tables.
--
-- Auction lifecycle:
--   open    → bidding (window open; bids accepted)
--          → closed  (close ledger reached; revealing)
--          → settled (winner paid, loser bonds refunded)
--          → defaulted (winner failed reveal/funds)
--          → fallback (no bid above reserve; handed to DEX routing
--                       then risk-reserve coverage)
--
-- A lot is created from a liquidation decision. Bidders submit sealed
-- commitments; the winning bid is revealed at close. Settlement is atomic
-- and idempotent (CAS-guarded). Losing bids' escrow (bond) is released.

CREATE TABLE IF NOT EXISTS auction_lots (
    id                  TEXT    PRIMARY KEY,
    -- The liquidation this lot covers (refs chain_liquidations.id).
    liquidation_id      TEXT    NOT NULL,
    -- The collateral asset being auctioned.
    collateral_asset    TEXT    NOT NULL,
    -- Quantity of collateral in the lot (10^7 fixed-point units).
    quantity            INTEGER NOT NULL CHECK (quantity > 0),
    -- Reserve price below which no bid wins (10^7 units of settlement asset).
    reserve_price       INTEGER NOT NULL CHECK (reserve_price >= 0),
    -- Bid bond required from each bidder (10^7 units).
    bid_bond            INTEGER NOT NULL CHECK (bid_bond >= 0),
    -- Auction window as ledger offsets from open_ledger.
    open_ledger         INTEGER NOT NULL,
    close_ledger        INTEGER NOT NULL CHECK (close_ledger > open_ledger),
    -- Position(s) being covered (JSON array of position ids).
    covered_positions   TEXT    NOT NULL DEFAULT '[]',
    -- Settlement asset the bids are denominated in (e.g. "USDC").
    settlement_asset    TEXT    NOT NULL,
    -- Status machine.
    status              TEXT    NOT NULL DEFAULT 'open'
                        CHECK (status IN
                            ('open','closed','settled','defaulted','fallback')),
    -- Winning bid id once settled.
    winning_bid_id      TEXT,
    -- Fallback path chosen when no winner ("dex_routing" | "risk_reserve").
    fallback_path       TEXT,
    -- Proceeds credited to the position's collateral account (10^7 units).
    proceeds            INTEGER NOT NULL DEFAULT 0,
    -- Idempotency: on-chain tx hash once settled.
    settle_tx_hash     TEXT,
    -- Operator who created the lot.
    created_by          TEXT    NOT NULL,
    created_at          TEXT    NOT NULL
                        DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    settled_at          TEXT,
    updated_at          TEXT    NOT NULL
                        DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE INDEX IF NOT EXISTS idx_auction_lots_status
    ON auction_lots (status, close_ledger);

CREATE INDEX IF NOT EXISTS idx_auction_lots_liquidation
    ON auction_lots (liquidation_id);

-- Sealed bids against a lot. Only the commitment is stored until reveal;
-- the revealed amount is recorded at close. Bond is held in escrow and
-- released on settle (winner's bond applies to payment; losers' refunded).
CREATE TABLE IF NOT EXISTS auction_bids (
    id                  TEXT    PRIMARY KEY,
    lot_id              TEXT    NOT NULL REFERENCES auction_lots(id),
    bidder              TEXT    NOT NULL,
    -- Commitment hash submitted during bidding (commit-reveal).
    commitment          TEXT    NOT NULL,
    -- Bid amount revealed at close (10^7 units). NULL until reveal.
    revealed_amount      INTEGER,
    -- Bid bond held in escrow (10^7 units).
    bond_amount         INTEGER NOT NULL DEFAULT 0,
    -- "pending" | "revealed" | "winning" | "losing" | "defaulted" | "refunded"
    status              TEXT    NOT NULL DEFAULT 'pending'
                        CHECK (status IN
                            ('pending','revealed','winning','losing','defaulted','refunded')),
    -- Whether the bond was refunded/forfeited.
    bond_status         TEXT    NOT NULL DEFAULT 'held'
                        CHECK (bond_status IN
                            ('held','refunded','forfeited','applied')),
    submitted_at        TEXT    NOT NULL
                        DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    revealed_at         TEXT,
    refunded_at         TEXT,
    created_at          TEXT    NOT NULL
                        DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE INDEX IF NOT EXISTS idx_auction_bids_lot
    ON auction_bids (lot_id, status);

CREATE INDEX IF NOT EXISTS idx_auction_bids_bidder
    ON auction_bids (bidder, status);

-- Audit trail of all bond movements (held, refunded, forfeited, applied).
CREATE TABLE IF NOT EXISTS auction_bond_movements (
    id                  TEXT    PRIMARY KEY,
    bid_id              TEXT    NOT NULL REFERENCES auction_bids(id),
    lot_id              TEXT    NOT NULL REFERENCES auction_lots(id),
    bidder              TEXT    NOT NULL,
    -- "held" | "refunded" | "forfeited" | "applied_to_payment"
    movement            TEXT    NOT NULL,
    amount              INTEGER NOT NULL,
    reason              TEXT    NOT NULL,
    ledger              INTEGER NOT NULL,
    occurred_at         TEXT    NOT NULL
                        DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE INDEX IF NOT EXISTS idx_bond_movements_bid
    ON auction_bond_movements (bid_id);

CREATE INDEX IF NOT EXISTS idx_bond_movements_lot
    ON auction_bond_movements (lot_id, occurred_at DESC);
