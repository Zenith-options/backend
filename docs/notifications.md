# Settlement lifecycle notifications

Issue #229. Code: `src/settlement_notifications/`. Schema:
`migrations/0012_settlement_notifications.sql`.

Traders are told whenever something happens to their money. Notifications go
out through **in-app**, **email** and **push**. Per-user preferences control
which channels are used, and each one is deduplicated per channel.

## Event catalogue

| Event family | Key | Severity | In-app | Email | Push |
|---|---|---|---|---|---|
| Expiry reminder (T-N) | `expiry_reminder_<N>h` | info (warning when N ≤ 1) | on | off | on |
| Settlement started | `settlement_started` | info | on | off | off |
| Settlement completed | `settlement_completed` | info | on | on | off |
| Settlement failed | `settlement_failed` | **critical** | always | on | on |
| Payout credited | `payout_credited` | info | on | on | on |
| Margin call | `margin_call` | **critical** | always | on | on |
| Liquidation warning | `liquidation_warning` | **critical** | always | on | on |
| Rollover executed | `rollover_executed` | info | on | on | off |

The on/off columns are the defaults (`preferences::default_enabled`).
"always" means the user cannot turn that channel off.

## Preference semantics

Preferences are stored per user in `notification_preferences`. For each
`(event, channel)` pair they resolve in this order:

1. **Critical events always reach the in-app inbox.** This cannot be overridden.
2. **Explicit overrides win.** Overrides are keyed `"<family>:<channel>"`,
   e.g. `"payout_credited:push": false`. A reminder override covers every
   horizon (T-24h, T-1h, ...).
3. **Global mute** suppresses every non-critical event on every channel.
   Critical events still go out on their enabled channels.
4. **Defaults** from the table above.
5. **Quiet hours** (local time, can wrap midnight) delay non-critical email
   and push until the window ends. In-app is never delayed, and neither are
   critical events.

A per-user **rate cap** (default 10 per hour per external channel) applies
to non-critical email and push. Notifications over the cap are recorded as
`suppressed` with reason `rate_capped`. Critical events are never capped.

## Delivery pipeline

* **Transactional outbox.** The settlement engine calls
  `settlement_notifications::notify(&mut tx, &signal, ..)` inside its own
  transaction. If the business change rolls back, no notification is written.
  `notify` does no network I/O, so a broken provider can never block or roll
  back settlement.
* **Deduplication.** The outbox has `UNIQUE (user_id, event_type, entity_id,
  channel)`. When a settlement batch is retried, or the same event arrives
  from several sources, the extra rows are simply not inserted.
* **Worker.** `Dispatcher::run_once` drains each channel concurrently and
  independently, with a timeout on every send. Rows are claimed
  optimistically, so two workers never send the same row. A channel that is
  down only reschedules its own rows.
* **Retries.** Transient failures retry with exponential backoff: 30s, 1m,
  2m ... capped at 1h, for up to 8 attempts. Permanent failures (4xx, no
  address on file) and exhausted retries are moved to `dead`.
* **Attempt log.** Every attempt is written to `notification_attempts` with
  its outcome (`delivered`, `transient_failure`, `permanent_failure`,
  `timeout`).

## Content safety

Templates can only use `TemplateData`. That is a closed set of typed fields:
instrument, the user's own amount and asset, a deadline, a roll target, and
a public failure reason. Every field is sanitised to a short allow-list of
characters. There is no free-text field, so internal errors, secrets and
other accounts' balances cannot reach a message body. Addresses (email,
push token) are looked up at send time through `AddressBook` and are never
stored in the outbox.

## Providers

| Channel | Provider | Configuration |
|---|---|---|
| in-app | `InAppInbox` (the outbox row is the inbox record) | none |
| email | `HttpEmailProvider` | `DELIVERY_EMAIL_API_URL`, `DELIVERY_EMAIL_API_TOKEN`, `DELIVERY_EMAIL_FROM` |
| push | `HttpPushProvider` | `NOTIFY_PUSH_GATEWAY_URL`, `NOTIFY_PUSH_GATEWAY_TOKEN` |

Out of scope: marketing broadcasts, and the machine-to-machine webhook
service.
