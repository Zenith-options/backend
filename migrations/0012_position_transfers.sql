-- Transfer offers are backend mirrors of contract-enforced position transfers.
-- Ownership is changed only after a trusted indexer confirms the contract event.
CREATE TABLE position_transfer_offers (
    id              TEXT PRIMARY KEY,
    position_id     TEXT NOT NULL REFERENCES positions(id),
    from_wallet     TEXT NOT NULL REFERENCES accounts(wallet_address),
    to_wallet       TEXT REFERENCES accounts(wallet_address),
    offer_type      TEXT NOT NULL CHECK (offer_type IN ('bilateral', 'listing')),
    asking_amount   INTEGER,
    asking_asset    TEXT,
    expires_at      TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'open'
                    CHECK (status IN ('open', 'accepted', 'cancelled', 'expired')),
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    CHECK (
        (offer_type = 'bilateral' AND to_wallet IS NOT NULL
            AND asking_amount IS NULL AND asking_asset IS NULL)
        OR
        (offer_type = 'listing' AND to_wallet IS NULL
            AND asking_amount > 0 AND asking_asset IS NOT NULL)
    )
);

CREATE UNIQUE INDEX position_transfer_one_open_offer
    ON position_transfer_offers(position_id) WHERE status = 'open';
CREATE INDEX position_transfer_listings
    ON position_transfer_offers(status, offer_type, expires_at);

CREATE TABLE position_transfer_confirmations (
    id              TEXT PRIMARY KEY,
    offer_id        TEXT NOT NULL UNIQUE REFERENCES position_transfer_offers(id),
    position_id     TEXT NOT NULL REFERENCES positions(id),
    from_wallet     TEXT NOT NULL REFERENCES accounts(wallet_address),
    to_wallet       TEXT NOT NULL REFERENCES accounts(wallet_address),
    chain_tx_hash   TEXT NOT NULL UNIQUE,
    confirmed_at    TEXT NOT NULL
);

-- Keep the existing immutable-position invariant except for an ownership
-- change backed by a confirmed transfer record. Position collateral and the
-- expiry/settlement fields remain on the same position row.
DROP TRIGGER IF EXISTS positions_immutable_columns;
CREATE TRIGGER positions_immutable_columns
BEFORE UPDATE ON positions
FOR EACH ROW
WHEN NEW.entry_premium IS NOT OLD.entry_premium
  OR NEW.opened_at IS NOT OLD.opened_at
  OR (
      NEW.wallet_address IS NOT OLD.wallet_address
      AND NOT EXISTS (
          SELECT 1 FROM position_transfer_confirmations c
          WHERE c.position_id = OLD.id
            AND c.from_wallet = OLD.wallet_address
            AND c.to_wallet = NEW.wallet_address
      )
  )
BEGIN
    SELECT RAISE(ABORT, 'position fields are immutable outside a confirmed transfer');
END;
