-- Settlement lifecycle notifications (issue #229).
--
-- Transactional outbox: business transactions (settlement, margin, rollover)
-- insert one `notification_outbox` row per (user, event, entity, channel) in
-- the same DB transaction that changes the business state. A worker drains
-- each channel independently, so a channel outage never blocks settlement or
-- the other channels. Timestamps are epoch milliseconds (UTC).

CREATE TABLE IF NOT EXISTS notification_preferences (
    user_id             TEXT    PRIMARY KEY,
    global_mute         INTEGER NOT NULL DEFAULT 0 CHECK (global_mute IN (0, 1)),
    -- Quiet hours, minutes after local midnight. Both NULL = no quiet hours.
    quiet_start_minute  INTEGER CHECK (quiet_start_minute BETWEEN 0 AND 1439),
    quiet_end_minute    INTEGER CHECK (quiet_end_minute BETWEEN 0 AND 1439),
    utc_offset_minutes  INTEGER NOT NULL DEFAULT 0,
    -- JSON object: {"<event_type>:<channel>": true|false, ...}
    overrides           TEXT    NOT NULL DEFAULT '{}',
    updated_at_ms       INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS notification_outbox (
    id                  TEXT    PRIMARY KEY,
    user_id             TEXT    NOT NULL,
    event_type          TEXT    NOT NULL,
    entity_id           TEXT    NOT NULL,
    channel             TEXT    NOT NULL CHECK (channel IN ('in_app', 'email', 'push')),
    severity            TEXT    NOT NULL CHECK (severity IN ('info', 'warning', 'critical')),
    title               TEXT    NOT NULL,
    body                TEXT    NOT NULL,
    status              TEXT    NOT NULL DEFAULT 'pending'
                        CHECK (status IN ('pending', 'delivered', 'dead', 'suppressed')),
    suppressed_reason   TEXT,
    attempts            INTEGER NOT NULL DEFAULT 0,
    next_attempt_at_ms  INTEGER NOT NULL,
    created_at_ms       INTEGER NOT NULL,
    delivered_at_ms     INTEGER,
    -- In-app only: when the user opened the notification.
    read_at_ms          INTEGER,
    last_error          TEXT,
    -- Cross-source / cross-retry deduplication key.
    UNIQUE (user_id, event_type, entity_id, channel)
);

CREATE INDEX IF NOT EXISTS idx_notification_outbox_due
    ON notification_outbox (channel, status, next_attempt_at_ms);
CREATE INDEX IF NOT EXISTS idx_notification_outbox_user_recent
    ON notification_outbox (user_id, channel, created_at_ms);

-- One row per delivery attempt, for support and audit.
CREATE TABLE IF NOT EXISTS notification_attempts (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    outbox_id           TEXT    NOT NULL REFERENCES notification_outbox(id),
    attempt             INTEGER NOT NULL,
    attempted_at_ms     INTEGER NOT NULL,
    outcome             TEXT    NOT NULL
                        CHECK (outcome IN ('delivered', 'transient_failure', 'permanent_failure', 'timeout')),
    error               TEXT
);

CREATE INDEX IF NOT EXISTS idx_notification_attempts_outbox
    ON notification_attempts (outbox_id, attempt);
