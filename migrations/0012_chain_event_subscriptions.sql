ALTER TABLE delivery_events ADD COLUMN event_key TEXT;

CREATE UNIQUE INDEX idx_delivery_events_wallet_type_key
    ON delivery_events(wallet_address, event_type, event_key)
    WHERE event_key IS NOT NULL;

CREATE TABLE watchlist_event_subscriptions (
    wallet_address TEXT NOT NULL,
    underlying TEXT NOT NULL,
    event_type TEXT NOT NULL CHECK (event_type IN (
        'settlement_completed',
        'margin_call_threshold',
        'liquidation_warning',
        'delivery_status',
        'vault_share_price_move'
    )),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (wallet_address, underlying, event_type),
    FOREIGN KEY (wallet_address, underlying)
        REFERENCES watchlist(wallet_address, underlying) ON DELETE CASCADE
);

CREATE INDEX idx_watchlist_event_subscriptions_event
    ON watchlist_event_subscriptions(event_type, underlying, wallet_address);
