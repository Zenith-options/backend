# Zenith security threat model

## Purpose and scope

This document describes the security assumptions and trust boundaries reviewers
should use when changing Zenith. It covers the Rust API and paper-trading
ledger, wallet authentication, keeper and signing components, Soroban
interactions, and the experimental oracle contract in `contracts/oracle`.
It complements the operational procedures in
[`secrets-rotation.md`](secrets-rotation.md) and the arithmetic scope in
[`verification.md`](verification.md); it is not a security audit or a claim
that the system is ready to custody funds.

The backend currently uses simulated market data by default, and the oracle
contract is not wired to the backend publisher or deployed. Contract controls
described below apply to that crate only. Any path that signs transactions or
holds collateral materially changes the impact of the threats and requires a
deployment-specific review before handling real value.

## Assets and security objectives

| Asset | Security objective |
|---|---|
| User funds and collateral (future on-chain scope) | Prevent unauthorized movement, incorrect settlement, and insolvency |
| Settlement prices and fixings | Preserve provenance, integrity, freshness, and an auditable history |
| Wallet identities and sessions | Bind actions to the intended wallet; prevent replay and cross-wallet access |
| Signing keys and service credentials | Keep private material out of logs and application memory where remote signing is available; support revocation and rotation |
| Ledger, positions, and keeper state | Preserve accounting invariants and prevent duplicate or unauthorized state transitions |
| Service availability and transaction budget | Limit abuse, resource exhaustion, and failure propagation |

## Actors and trust boundaries

- **Clients and wallets** are untrusted. Requests, supplied legs, symbols,
  signatures, and retry behavior must be treated as attacker-controlled.
- **The API process** validates requests and coordinates the SQLite-backed
  paper ledger. Its host, build artifact, runtime configuration, and service
  identity are trusted only to the extent that deployment controls protect
  them.
- **The database** is a persistence boundary, not an authorization authority.
  Database access or file tampering can change ledger state and must be treated
  as a compromise.
- **Keeper workers and relayers** can initiate privileged or fee-paying
  transactions. Their permissions, target contracts, fee ceilings, and
  duplicate-execution behavior must be narrowly scoped.
- **Oracle publisher keys and price sources** are trusted inputs to settlement.
  A valid signature authenticates the configured signer; it does not prove
  that the signer used a correct or independent market price.
- **Soroban contracts and Stellar RPC/Horizon services** are external trust
  boundaries. RPC responses can be unavailable, delayed, or inconsistent;
  transaction simulation is not a substitute for on-chain authorization.
- **Secret providers, CI, dependencies, and operators** can affect deployed
  code or credentials and are part of the trusted computing base.

## Threats, current controls, and residual risk

| Threat | Existing controls or intended behavior | Residual risk / required review |
|---|---|---|
| Forged or replayed wallet authentication | Single-use, expiring sign-in nonces and bearer sessions | Review nonce/session storage, revocation, domain binding, and rate limits whenever auth changes |
| Cross-wallet access or ledger corruption | Authorization is wallet-scoped; property-based state-machine tests exercise ledger invariants | Tests do not protect against database tampering, concurrency gaps, or untested authorization paths |
| Incorrect or stale settlement price | The oracle contract verifies an authorized Ed25519 signature, accepts only positive prices, enforces expiry through a five-minute publication window, and makes each `(underlying, expiry)` fixing immutable | One signer can still publish an incorrect positive price. There is no aggregation, deviation bound, quorum, source attestation, or backend publisher integration. Do not use it with real funds without independently reviewed source policy and operational controls |
| Reuse of a fixing signature across deployments | The publisher signs a canonical fixing tuple and the contract verifies it | The v1 message has no network or contract domain. Use deployment-specific keys and do not reuse signatures across environments; add domain separation before any multi-deployment production use |
| Oracle admin or publisher key compromise | The contract separates admin authority from publisher authority; publisher keys can be rotated; secrets can use Vault Transit or AWS KMS | The contract has one publisher key and a single admin address, with no quorum, timelock, or emergency pause. Key compromise can authorize arbitrary prices in the allowed window |
| Keeper or relayer abuse | Keeper work is tracked and the relayer has a sponsorship policy boundary | Treat keeper credentials as transaction authority. Require least-privilege contract permissions, explicit fee limits, idempotency, leader coordination, and alerting before enabling real transactions |
| Signing key or credential exposure | `SecretStore` supports environment, SOPS, and Vault providers; remote KMS signers keep private keys outside the process; rotation procedures are documented | Environment variables and operator/CI access remain sensitive. Do not use local in-memory signers for production keys; test revocation and incident response |
| RPC outage, stale reads, or misleading simulation | RPC errors are surfaced; readiness and transaction simulation are available | Do not treat simulated success or a single RPC read as finality. Production submission needs confirmation policy, retry bounds, reconciliation, and alerting |
| Denial of service or resource exhaustion | API rate limiting and Soroban transaction budget constraints are used in relevant paths | Reassess limits, database contention, WebSocket fanout, and paid transaction exposure under production traffic |
| Dependency, build, or deployment compromise | CI tests and Kani arithmetic proofs run on changes; secrets are segregated from source | These controls do not prove contract behavior or artifact provenance. Pin/review dependencies, protect release credentials, verify deployed WASM, and independently audit fund-bearing code |

## Required decisions before real funds

Before enabling collateral custody, settlement, or production signing, reviewers
and operators must establish and document:

1. Which independent data sources determine each fixing, how outliers and
   missing feeds are handled, and who can authorize an override.
2. Whether oracle publication requires multiple independent signers, and the
   price, age, and deviation bounds enforced on-chain.
3. How oracle signatures are bound to the network, contract, asset, expiry,
   and price; how signing keys are isolated, rotated, revoked, and recovered.
4. Which keeper actions are permitted, the maximum fees and transaction scope,
   how competing workers are fenced, and how duplicate or reverted work is
   reconciled.
5. Who controls contract administration and upgrades, how emergency pause and
   recovery work, and whether deployed code is immutable and reproducible.
6. How RPC finality, storage archival/TTL, monitoring, incident escalation,
   and database recovery are handled in the target deployment.

The current oracle crate is a signed-fixing storage primitive, not a complete
market-price oracle. Its single-signer trust model and current signature format
are deliberately visible constraints. A completed checklist does not replace
independent contract and deployment security review.

## Related documents

- [`secrets-rotation.md`](secrets-rotation.md) — secret and signer rotation
- [`verification.md`](verification.md) — what arithmetic properties are and
  are not formally verified
- [`../contracts/oracle/README.md`](../contracts/oracle/README.md) — oracle
  contract scope, policy, and build commands
