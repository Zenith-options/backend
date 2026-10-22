# Zenith Soroban oracle

`zenith-oracle` stores signed settlement fixings keyed by `(underlying,
expiry)`. Only the configured Ed25519 publisher key can publish. Each fixing
must have a positive `i128` price, use a short uppercase symbol, and be
published from expiry through the following five minutes. A fixing is
immutable once stored; `get_fixing` serves it by key. The admin can rotate the
publisher key or transfer admin authority.

Signatures use the backend's `zenith:fixing:v1|<underlying>|<expiry>|<price>`
UTF-8 message. Contract state includes the fixing's publication timestamp and
emits an event on publication. Persistent entries are extended for up to
30 days on publish and read; deployments must account for Soroban storage TTL
and archival when designing long-term settlement history.

This crate does not connect to the backend publisher, choose or aggregate
market-data sources, or provide a production deployment configuration. The
single publisher key and five-minute policy are explicit trust assumptions,
not a claim that an oracle is safe to use with real funds. Review
[`../../docs/threat-model.md`](../../docs/threat-model.md) before deployment.

## Build and test

```bash
cargo test -p zenith-oracle
cargo build -p zenith-oracle --target wasm32-unknown-unknown --release
```
