-- Stop-loss / take-profit / stop-limit trigger orders.
--
-- Status machine:
--   armed → triggered → executing → filled
--                    ↘ rejected
--   armed → cancelled
--   armed → expired

CREATE TABLE IF NOT EXISTS trigger_orders (
    id                  TEXT PRIMARY KEY,
    wallet_address      TEXT NOT NULL REFERENCES accounts(wallet_address),
    -- "stop_loss" | "take_profit" | "stop_limit"
    trigger_type        TEXT NOT NULL
                        CHECK (trigger_type IN ('stop_loss','take_profit','stop_limit')),
    -- "close_long" | "close_short" | "open_long" | "open_short"
    side                TEXT NOT NULL,
    -- underlying symbol, e.g. "BTC"
    underlying          TEXT NOT NULL,
    -- position id this trigger is attached to (NULL = standalone)
    position_id         TEXT,
    -- price at which the trigger fires (10^7 fixed-point)
    trigger_price       INTEGER NOT NULL CHECK (trigger_price > 0),
    -- for stop_limit: the limit price to submit; NULL for market stops
    limit_price         INTEGER,
    -- size in contracts (10^7 fixed-point)
    size                INTEGER NOT NULL CHECK (size > 0),
    -- "gtc" | "ioc" | "fok"
    time_in_force       TEXT NOT NULL DEFAULT 'gtc',
    -- max slippage fraction (e.g. 0.02 = 2%), stored as INTEGER * 1e6
    slippage_bps        INTEGER NOT NULL DEFAULT 200,  -- 2 %
    -- current status
    status              TEXT NOT NULL DEFAULT 'armed'
                        CHECK (status IN ('armed','triggered','executing','filled',
                                          'cancelled','expired','rejected')),
    -- idempotency key used for the settlement submission (set on trigger)
    idempotency_key     TEXT UNIQUE,
    -- oracle price that fired the trigger (10^7)
    fired_at_price      INTEGER,
    -- wall-clock timestamps (ISO-8601 UTC)
    armed_at            TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    triggered_at        TEXT,
    filled_at           TEXT,
    expires_at          TEXT,           -- NULL = never expires
    -- audit: last amendment note
    amended_at          TEXT,
    amendment_note      TEXT,
    -- price source used for evaluation ("oracle" | "mark" | "simulator")
    price_source        TEXT NOT NULL DEFAULT 'simulator',
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

-- Fast armed-trigger lookup for the evaluator sweep.
CREATE INDEX IF NOT EXISTS idx_triggers_armed
    ON trigger_orders (underlying, status)
    WHERE status = 'armed';

-- Per-wallet history.
CREATE INDEX IF NOT EXISTS idx_triggers_wallet
    ON trigger_orders (wallet_address, created_at DESC);

-- Position linkage (auto-cancel on position close).
CREATE INDEX IF NOT EXISTS idx_triggers_position
    ON trigger_orders (position_id)
    WHERE position_id IS NOT NULL;

-- Amendment audit log.
CREATE TABLE IF NOT EXISTS trigger_amendments (
    id              TEXT PRIMARY KEY,
    trigger_id      TEXT NOT NULL REFERENCES trigger_orders(id),
    field           TEXT NOT NULL,  -- "trigger_price" | "limit_price" | "size"
    old_value       TEXT NOT NULL,
    new_value       TEXT NOT NULL,
    amended_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    amended_by      TEXT NOT NULL   -- wallet_address
);

CREATE INDEX IF NOT EXISTS idx_trigger_amendments_trigger
    ON trigger_amendments (trigger_id, amended_at DESC);
