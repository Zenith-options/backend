# Options Pricing API

A high-performance options pricing and market data API.

## API

### REST

- `GET /api/v1/spot` — current spot prices for all underlyings.
- `GET /api/v1/chain` — option chain for an underlying/expiry.
- `GET /api/v1/surface` — implied volatility surface for an underlying.

### WebSocket

#### Legacy: `/api/v1/ws/spot`

> **Deprecation note:** `/api/v1/ws/spot` is deprecated in favor of the
> multiplexed `/api/v1/ws` endpoint below. It continues to work unchanged for
> backwards compatibility, but new clients should use `/api/v1/ws`.

Pushes every underlying's spot price to every connected client roughly every
2 seconds, regardless of what the client is interested in.

#### Multiplexed: `/api/v1/ws`

A single connection can subscribe to multiple public channels using a JSON
subscribe/unsubscribe protocol. Each channel delivers a snapshot followed by
deltas, with per-channel monotonically increasing sequence numbers so clients
can detect gaps and resubscribe for a fresh snapshot.

**Client messages**

```json
{"op": "subscribe", "channels": ["spot.BTC", "chain.BTC.2024-12-27", "surface.BTC"]}
{"op": "unsubscribe", "channels": ["spot.BTC"]}
```

**Server messages**

```json
{"channel": "spot.BTC", "seq": 1, "type": "snapshot", "data": { ... }}
{"channel": "spot.BTC", "seq": 2, "type": "update", "data": { ... }}
```

**Channels**

- `spot.<UNDERLYING>` — spot price updates for an underlying.
- `chain.<UNDERLYING>.<EXPIRY>` — option chain updates for an underlying/expiry.
- `surface.<UNDERLYING>` — implied volatility surface updates for an underlying.

**Behavior**

- Per-channel `seq` is monotonically increasing. A gap indicates missed
  messages; resubscribe to receive a fresh snapshot.
- Chain updates are computed lazily and shared across subscribers: a channel is
  only computed when it has at least one subscriber.
- A maximum of 50 subscriptions per connection is allowed. Exceeding the limit
  returns a structured error message rather than disconnecting.
- Subscribing to an unknown underlying returns an error message, not a
  disconnect.
- Duplicate subscriptions are idempotent.
- Connections that never send a subscribe are closed after a 60s idle timeout.

**Out of scope:** private/authenticated channels (tracked separately).

See [`docs/ws-protocol.md`](docs/ws-protocol.md) for the full protocol write-up.
