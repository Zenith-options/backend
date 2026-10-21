-- Outbound webhooks for settlement lifecycle events, delivered in order per
-- subscription with durable retries. See src/settlement_webhooks.rs.
CREATE TABLE settlement_webhook_subscriptions (
    id               TEXT PRIMARY KEY,
    url              TEXT NOT NULL,
    secret           TEXT NOT NULL,
    -- Comma-separated event types, e.g. 'settlement.confirmed,settlement.failed'.
    event_types      TEXT NOT NULL,
    status           TEXT NOT NULL DEFAULT 'active'
                     CHECK (status IN ('active', 'suspended', 'disabled')),
    status_reason    TEXT,
    -- Sequence number the next event for this subscription gets.
    next_seq         INTEGER NOT NULL DEFAULT 1,
    -- One dispatcher per subscription at a time, to keep delivery ordered.
    lease_owner      TEXT,
    lease_until_ms   INTEGER,
    created_at_ms    INTEGER NOT NULL,
    updated_at_ms    INTEGER NOT NULL
);

CREATE TABLE settlement_events (
    id            TEXT PRIMARY KEY,
    event_type    TEXT NOT NULL CHECK (event_type IN (
                      'settlement.submitted', 'settlement.confirmed', 'settlement.failed',
                      'settlement.expired', 'settlement.liquidated')),
    settlement_id TEXT NOT NULL,
    payload       TEXT NOT NULL,
    -- Publishing the same lifecycle transition twice is a no-op.
    dedupe_key    TEXT NOT NULL UNIQUE,
    created_at_ms INTEGER NOT NULL
);

CREATE INDEX idx_settlement_events_settlement ON settlement_events(settlement_id, created_at_ms);

CREATE TABLE settlement_webhook_deliveries (
    id                 TEXT PRIMARY KEY,
    subscription_id    TEXT NOT NULL REFERENCES settlement_webhook_subscriptions(id) ON DELETE CASCADE,
    seq                INTEGER NOT NULL,
    event_id           TEXT NOT NULL REFERENCES settlement_events(id),
    status             TEXT NOT NULL DEFAULT 'pending'
                       CHECK (status IN ('pending', 'delivered', 'dead')),
    attempts           INTEGER NOT NULL DEFAULT 0,
    next_attempt_at_ms INTEGER NOT NULL,
    last_status_code   INTEGER,
    last_error         TEXT,
    delivered_at_ms    INTEGER,
    created_at_ms      INTEGER NOT NULL,
    UNIQUE (subscription_id, seq),
    UNIQUE (subscription_id, event_id)
);

-- Head-of-line lookup: lowest undelivered sequence per subscription.
CREATE INDEX idx_settlement_webhook_deliveries_head
    ON settlement_webhook_deliveries(subscription_id, status, seq);
