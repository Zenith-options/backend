-- Chain cost attribution and budget guardrails (issue #230).
--
-- Every on-chain operation appends one row to `chain_cost_events` when its
-- submission result is known (including failed and preflight-rejected
-- attempts). Amounts are stroops (1 XLM = 10^7). Timestamps are epoch ms UTC.

CREATE TABLE IF NOT EXISTS chain_cost_events (
    id                  TEXT    PRIMARY KEY,
    recorded_at_ms      INTEGER NOT NULL,
    operation           TEXT    NOT NULL,   -- submission | fee_bump | restore_footprint | extend_ttl | storage_write | upload_wasm
    status              TEXT    NOT NULL,   -- success | failed_on_chain | preflight_rejected | not_included
    tx_hash             TEXT,
    ledger              INTEGER,
    -- Attribution to the originating business action.
    action              TEXT    NOT NULL,   -- settlement_batch | vault_deposit | oracle_publish | keeper_run | emergency_pause | admin | other
    action_id           TEXT    NOT NULL,
    request_id          TEXT,
    job_id              TEXT,
    account             TEXT,
    series              TEXT,
    criticality         TEXT    NOT NULL CHECK (criticality IN ('critical', 'non_critical')),
    -- Cost breakdown. net_cost is the true post-refund cost.
    max_fee_bid         INTEGER NOT NULL DEFAULT 0,
    inclusion_fee       INTEGER NOT NULL DEFAULT 0,
    resource_fee        INTEGER NOT NULL DEFAULT 0,
    rent_fee            INTEGER NOT NULL DEFAULT 0,
    refund              INTEGER NOT NULL DEFAULT 0,
    net_cost            INTEGER NOT NULL DEFAULT 0,
    -- 0 when the reported fee_charged disagreed with the computed breakdown
    -- (net_cost then trusts fee_charged).
    reconciled          INTEGER NOT NULL DEFAULT 1,
    reservation_id      TEXT
);

CREATE INDEX IF NOT EXISTS idx_chain_cost_events_action
    ON chain_cost_events (action, action_id);
CREATE INDEX IF NOT EXISTS idx_chain_cost_events_time
    ON chain_cost_events (recorded_at_ms);
CREATE INDEX IF NOT EXISTS idx_chain_cost_events_account
    ON chain_cost_events (account, recorded_at_ms);
CREATE INDEX IF NOT EXISTS idx_chain_cost_events_series
    ON chain_cost_events (series, recorded_at_ms);

-- Pre-computed daily rollup, updated in the same transaction as each event.
CREATE TABLE IF NOT EXISTS chain_cost_daily_rollup (
    day_index           INTEGER NOT NULL,   -- days since 1970-01-01 UTC
    action              TEXT    NOT NULL,
    operation           TEXT    NOT NULL,
    tx_count            INTEGER NOT NULL DEFAULT 0,
    inclusion_fee       INTEGER NOT NULL DEFAULT 0,
    resource_fee        INTEGER NOT NULL DEFAULT 0,
    rent_fee            INTEGER NOT NULL DEFAULT 0,
    refund              INTEGER NOT NULL DEFAULT 0,
    net_cost            INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (day_index, action, operation)
);

-- Budget per cost category (= business action) and period.
CREATE TABLE IF NOT EXISTS chain_budget_policies (
    category            TEXT    PRIMARY KEY,
    period              TEXT    NOT NULL CHECK (period IN ('daily', 'weekly')),
    limit_stroops       INTEGER NOT NULL CHECK (limit_stroops >= 0),
    warn_ratio_bps      INTEGER NOT NULL DEFAULT 8000 CHECK (warn_ratio_bps BETWEEN 0 AND 10000),
    throttle_per_minute INTEGER NOT NULL DEFAULT 10,
    updated_at_ms       INTEGER NOT NULL DEFAULT 0
);

-- Durable budget counters. `reserved` holds in-flight submissions so
-- concurrent workers cannot overshoot the limit.
CREATE TABLE IF NOT EXISTS chain_budget_periods (
    category            TEXT    NOT NULL,
    period_start_ms     INTEGER NOT NULL,
    spent               INTEGER NOT NULL DEFAULT 0,
    reserved            INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (category, period_start_ms)
);

CREATE TABLE IF NOT EXISTS chain_budget_reservations (
    id                  TEXT    PRIMARY KEY,
    category            TEXT    NOT NULL,
    period_start_ms     INTEGER NOT NULL,
    amount              INTEGER NOT NULL,
    criticality         TEXT    NOT NULL CHECK (criticality IN ('critical', 'non_critical')),
    created_at_ms       INTEGER NOT NULL,
    settled_at_ms       INTEGER,
    actual_cost         INTEGER
);

CREATE INDEX IF NOT EXISTS idx_chain_budget_reservations_open
    ON chain_budget_reservations (category, settled_at_ms, created_at_ms);

-- Every guardrail trip, once per (category, period, level).
CREATE TABLE IF NOT EXISTS chain_budget_breaches (
    category            TEXT    NOT NULL,
    period_start_ms     INTEGER NOT NULL,
    level               TEXT    NOT NULL CHECK (level IN ('warn', 'hard_stop', 'critical_overrun')),
    committed           INTEGER NOT NULL,
    limit_stroops       INTEGER NOT NULL,
    at_ms               INTEGER NOT NULL,
    PRIMARY KEY (category, period_start_ms, level)
);
