-- Margin call lifecycle table.
-- Each row tracks one open/closed call against an account with its full
-- audit trail: when it was opened, what the deficit was, what the required
-- top-up was, when the cure window closes (expressed as both wall-clock and
-- ledger sequence so on-chain / off-chain settlement agree), and how it
-- terminated.

CREATE TABLE IF NOT EXISTS margin_calls (
    id                TEXT PRIMARY KEY,
    wallet_address    TEXT    NOT NULL,
    -- triggering deficit in scaled fixed-point (10^7 units)
    deficit           INTEGER NOT NULL,
    -- amount of collateral the account must add to cure (10^7 units)
    required_top_up   INTEGER NOT NULL,
    -- wall-clock timestamps (ISO-8601 UTC)
    opened_at         TEXT    NOT NULL,
    cure_deadline     TEXT    NOT NULL,
    -- ledger sequence at which the call was opened / at which it expires
    opened_at_ledger  INTEGER NOT NULL,
    deadline_ledger   INTEGER NOT NULL,
    -- current lifecycle status: open | cured | expired | escalated | partially_cured
    status            TEXT    NOT NULL DEFAULT 'open',
    -- terminal outcome detail (NULL while open)
    resolved_at       TEXT,
    resolved_at_ledger INTEGER,
    -- amount actually topped up when status = cured | partially_cured (10^7 units)
    cured_amount      INTEGER,
    -- freeform note written by the state machine or an operator override
    note              TEXT,
    -- prevent call spam: timestamp of the previous call on this account
    previous_call_id  TEXT,
    created_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- Fast lookup for the monitor: all open calls that have not yet expired.
CREATE INDEX IF NOT EXISTS idx_margin_calls_open
    ON margin_calls (wallet_address)
    WHERE status = 'open';

-- Deadline sweep: all open calls ordered by deadline ascending so the
-- sweep job processes the soonest-expiring calls first.
CREATE INDEX IF NOT EXISTS idx_margin_calls_deadline
    ON margin_calls (cure_deadline)
    WHERE status = 'open';

-- Audit / history API: account's full call history.
CREATE INDEX IF NOT EXISTS idx_margin_calls_wallet_opened
    ON margin_calls (wallet_address, opened_at DESC);
