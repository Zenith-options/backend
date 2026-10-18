# Chain operations and safety

`/health` remains process liveness. Chain readiness combines hysteretic RPC probes, signer reachability, and indexer lag. Metrics use bounded labels only; correlation IDs link jobs to hashes in records and traces without using either as metric labels.

Privileged signing is default-deny: the contract and method must be allowlisted and fee, per-transaction amount, and per-asset rolling spend must remain below configured ceilings. `CHAIN_SUBMISSIONS_ENABLED=false` is an immediate kill switch; ingestion stays active and already-broadcast transactions may still confirm.

`CHAIN_MODE=paper` performs no RPC requests, `shadow` indexes and compares without changing user state, and `live` treats chain state as authoritative. Live mode fails closed during degradation and never falls back to paper accounting.

Start a disposable network with `make chain-local-up`, then deploy pinned contract WASM using a preconfigured secret-backed Stellar identity with `make chain-local-deploy`. The generated JSON descriptor is the contract registry input.
