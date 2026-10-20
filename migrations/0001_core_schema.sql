-- Consolidated core schema (SQLite).
--
-- This migration replaces the previously split, mutually-incompatible set of
-- migration files (which mixed several schema lineages — e.g. `contracts`
-- vs `quantity`, REAL vs scaled-INTEGER money — and Postgres-only DDL). The
-- original files are preserved under `migrations/archive/` for reference.
--
-- Monetary values (`Money`) and contract quantities (`Qty`) are stored as
-- INTEGER values scaled by 10^7 (Stellar stroops precision), matching the
-- sqlx `Money`/`Qty` type implementations in `src/models.rs`.

-- ─── Accounts ────────────────────────────────────────────────────────────────
-- Paper-trading account, created lazily on first auth. `balance` and
-- `collateral_locked` are scaled by 10^7.
CREATE TABLE accounts (
    wallet_address      TEXT PRIMARY KEY,
    balance             INTEGER NOT NULL DEFAULT 1000000000000 CHECK (balance >= 0), -- 100000.0 * 10^7
    collateral_locked   INTEGER NOT NULL DEFAULT 0 CHECK (collateral_locked >= 0),
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- ─── Account epochs ──────────────────────────────────────────────────────────
-- A reset archives the prior trading session under its old epoch; the default
-- history view reflects the live epoch, `?epoch=all` spans every epoch.
CREATE TABLE account_epochs (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    wallet_address  TEXT NOT NULL REFERENCES accounts(wallet_address),
    is_current      INTEGER NOT NULL DEFAULT 1,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX idx_account_epochs_wallet ON account_epochs(wallet_address, is_current);

-- ─── Positions ───────────────────────────────────────────────────────────────
-- Open and closed option positions. A position starts `open` and transitions
-- to `closed`/`rolled` — never deleted, so this doubles as the trade ledger.
-- `strike`, `entry_premium`, `entry_spot`, `collateral`, `close_premium`,
-- `close_spot`, `realized_pnl` and `contracts` are scaled by 10^7.
CREATE TABLE positions (
    id              TEXT PRIMARY KEY,
    wallet_address  TEXT NOT NULL REFERENCES accounts(wallet_address),
    underlying      TEXT NOT NULL,
    strike          INTEGER NOT NULL CHECK (strike > 0),
    expiry_days     REAL NOT NULL,
    -- Absolute expiry timestamp. Kept for API compatibility; the pricing
    -- path derives time-to-expiry from `expiry_days`, so inserts may rely on
    -- this default.
    expires_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    option_type     TEXT NOT NULL CHECK (option_type IN ('call', 'put')),
    position_type   TEXT NOT NULL CHECK (position_type IN ('long', 'short')),
    contracts       INTEGER NOT NULL CHECK (contracts > 0),
    entry_premium   INTEGER NOT NULL,
    entry_spot      INTEGER NOT NULL,
    collateral      INTEGER NOT NULL DEFAULT 0,
    status          TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'closed', 'rolled')),
    close_premium   INTEGER,
    close_spot      INTEGER,
    realized_pnl    INTEGER,
    opened_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    closed_at       TEXT,
    strategy_id     TEXT,
    epoch_id        INTEGER,
    -- An open position must not carry settlement columns; a settled
    -- (closed/rolled) position must carry all of them.
    CHECK (
        (status = 'open'
            AND close_premium IS NULL AND close_spot IS NULL
            AND realized_pnl IS NULL AND closed_at IS NULL)
        OR
        (status IN ('closed', 'rolled')
            AND close_premium IS NOT NULL AND close_spot IS NOT NULL
            AND realized_pnl IS NOT NULL AND closed_at IS NOT NULL)
    )
);

CREATE INDEX idx_positions_wallet ON positions(wallet_address);
CREATE INDEX idx_positions_wallet_status ON positions(wallet_address, status);
CREATE INDEX idx_positions_strategy ON positions(strategy_id) WHERE strategy_id IS NOT NULL;
CREATE INDEX idx_positions_epoch ON positions(wallet_address, epoch_id);
-- Keyset-pagination index for the trade history: (closed_at DESC, id DESC).
CREATE INDEX idx_positions_history ON positions(wallet_address, status, closed_at DESC, id DESC);

-- ─── Watchlist ───────────────────────────────────────────────────────────────
CREATE TABLE watchlist (
    wallet_address  TEXT NOT NULL REFERENCES accounts(wallet_address),
    underlying      TEXT NOT NULL,
    added_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (wallet_address, underlying)
);

-- ─── Alerts ──────────────────────────────────────────────────────────────────
-- Alert conditions span simple price thresholds plus derived metrics
-- (percent change, IV, position/strategy P&L, portfolio delta, expiry
-- proximity). `target_price` stays REAL because it is compared directly
-- against live spot values in SQL.
CREATE TABLE alerts (
    id                  TEXT PRIMARY KEY,
    wallet_address      TEXT NOT NULL REFERENCES accounts(wallet_address),
    underlying          TEXT NOT NULL,
    condition           TEXT NOT NULL CHECK (condition IN (
        'above', 'below',
        'percent_change_above', 'percent_change_below',
        'iv_above', 'iv_below',
        'position_pnl_above', 'position_pnl_below',
        'strategy_pnl_above', 'strategy_pnl_below',
        'portfolio_delta_above', 'portfolio_delta_below', 'portfolio_delta_outside',
        'expiry_within'
    )),
    target_price        REAL NOT NULL,
    triggered           INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    triggered_at        TEXT,
    window_seconds      INTEGER,
    strike              REAL,
    expiry_days         REAL,
    option_type         TEXT CHECK (option_type IS NULL OR option_type IN ('call', 'put')),
    position_id         TEXT,
    strategy_id         TEXT,
    trigger_policy      TEXT NOT NULL DEFAULT 'once' CHECK (trigger_policy IN ('once', 'recurring', 'auto_rearm')),
    cooldown_seconds    INTEGER NOT NULL DEFAULT 0 CHECK (cooldown_seconds >= 0),
    last_triggered_at   TEXT,
    lower_bound         REAL,
    upper_bound         REAL
);

CREATE INDEX idx_alerts_wallet ON alerts(wallet_address);
CREATE INDEX idx_alerts_untriggered ON alerts(underlying, triggered) WHERE triggered = 0;
CREATE INDEX idx_alerts_due ON alerts(triggered, trigger_policy, last_triggered_at);

-- Rolling spot history used by percent-change and windowed alert conditions.
CREATE TABLE spot_history (
    underlying  TEXT NOT NULL,
    price       REAL NOT NULL,
    recorded_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (underlying, recorded_at)
);
CREATE INDEX idx_spot_history_lookup ON spot_history(underlying, recorded_at);

-- ─── Auth: nonces, sessions, session IPs ─────────────────────────────────────
CREATE TABLE auth_nonces (
    nonce           TEXT PRIMARY KEY,
    wallet_address  TEXT NOT NULL,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    expires_at      TEXT NOT NULL
);

CREATE TABLE sessions (
    token           TEXT PRIMARY KEY,
    wallet_address  TEXT NOT NULL REFERENCES accounts(wallet_address),
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    expires_at      TEXT NOT NULL
);

CREATE INDEX idx_sessions_wallet ON sessions(wallet_address);

CREATE TABLE session_ips (
    wallet_address  TEXT NOT NULL,
    ip              TEXT NOT NULL,
    session_token   TEXT NOT NULL,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX idx_session_ips_wallet ON session_ips(wallet_address);

-- ─── API keys ────────────────────────────────────────────────────────────────
CREATE TABLE api_keys (
    id              TEXT PRIMARY KEY,
    wallet_address  TEXT NOT NULL REFERENCES accounts(wallet_address),
    secret          TEXT NOT NULL,
    scopes          TEXT NOT NULL,
    ip_allowlist    TEXT,
    expires_at      TEXT,
    label           TEXT NOT NULL,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX idx_api_keys_wallet ON api_keys(wallet_address);

-- ─── Transactional outbox ────────────────────────────────────────────────────
-- Domain events are written in the same transaction as the state change that
-- produced them; a relay polls and dispatches them with per-consumer offsets.
CREATE TABLE outbox (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    aggregate_type  TEXT NOT NULL,
    aggregate_id    TEXT NOT NULL,
    event_type      TEXT NOT NULL,
    payload         TEXT NOT NULL,
    published_at    TEXT,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX idx_outbox_unpublished ON outbox(published_at) WHERE published_at IS NULL;
CREATE INDEX idx_outbox_aggregate ON outbox(aggregate_type, aggregate_id);

CREATE TABLE outbox_consumer_offsets (
    consumer        TEXT PRIMARY KEY,
    last_event_id   INTEGER NOT NULL DEFAULT 0,
    updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE outbox_dedupe (
    consumer        TEXT NOT NULL,
    event_id        INTEGER NOT NULL,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (consumer, event_id)
);

-- ─── Position integrity triggers ─────────────────────────────────────────────
-- The CHECK constraints above enforce per-row coherence; these triggers
-- enforce the state-machine and immutability rules that span rows/time.

-- Legal status transitions only: open -> closed|rolled, closed -> rolled.
-- Everything else (reopening a settled position, un-rolling, etc.) is
-- rejected. `IS NOT` is used so NULLs compare correctly.
CREATE TRIGGER trg_positions_status_transition
BEFORE UPDATE OF status ON positions
FOR EACH ROW
WHEN NOT (
    (OLD.status = 'open' AND NEW.status IN ('closed', 'rolled'))
    OR (OLD.status = 'closed' AND NEW.status = 'rolled')
)
BEGIN
    SELECT RAISE(ABORT, 'illegal position status transition');
END;

-- Identity and entry terms are written once at open and never change. A
-- settlement write only touches status + the close/realized columns, so it
-- is unaffected.
CREATE TRIGGER trg_positions_immutable_columns
BEFORE UPDATE ON positions
FOR EACH ROW
WHEN OLD.id IS NOT NEW.id
   OR OLD.wallet_address IS NOT NEW.wallet_address
   OR OLD.underlying IS NOT NEW.underlying
   OR OLD.strike IS NOT NEW.strike
   OR OLD.expiry_days IS NOT NEW.expiry_days
   OR OLD.expires_at IS NOT NEW.expires_at
   OR OLD.option_type IS NOT NEW.option_type
   OR OLD.position_type IS NOT NEW.position_type
   OR OLD.contracts IS NOT NEW.contracts
   OR OLD.entry_premium IS NOT NEW.entry_premium
   OR OLD.entry_spot IS NOT NEW.entry_spot
   OR OLD.collateral IS NOT NEW.collateral
   OR OLD.opened_at IS NOT NEW.opened_at
   OR OLD.strategy_id IS NOT NEW.strategy_id
   OR OLD.epoch_id IS NOT NEW.epoch_id
BEGIN
    SELECT RAISE(ABORT, 'position immutable column modified');
END;
