# Soroban network upgrade runbook

How to take the backend through a scheduled Stellar/Soroban protocol upgrade
without running chain mode or deploying contracts against a protocol the
settlement contracts and the backend haven't been verified on.

The checks live in `src/soroban/protocol.rs`. Every check is recorded in the
`protocol_observations` table.

## What is compared

| Source | Field | Meaning |
|---|---|---|
| `getLatestLedger` | `protocolVersion` | protocol the network is closing ledgers at |
| `getVersionInfo` / `getNetwork` | `protocolVersion` | protocol the RPC build targets |
| `getNetwork` | `passphrase` | which network the RPC is on |
| `ZENITH_CONTRACT_MIN_PROTOCOL` / `ZENITH_CONTRACT_MAX_PROTOCOL` | — | protocol range the compiled contract SDK (`ZENITH_CONTRACT_SDK_VERSION`) is verified for |
| contract WASM `contractenvmetav0` | interface version | protocol a specific WASM was built for (checked on deploy) |

The gate decides:

* **Chain-mode startup** is refused unless a check from the last
  `ZENITH_PROTOCOL_REPORT_MAX_AGE_SECS` (default 600) found nothing `fatal`.
* **Contract deployment** is refused unless that check found nothing at all
  that blocks deploys, *and* the WASM's interface protocol is `<=` the network
  protocol, inside the SDK range, and not a pre-release build.

## Configuration

| Variable | Default | Purpose |
|---|---|---|
| `ZENITH_EXPECTED_NETWORK_PASSPHRASE` | unset | refuse to run if the RPC is on a different network |
| `ZENITH_CONTRACT_SDK_VERSION` | `22.0.0` | informational, shown in findings |
| `ZENITH_CONTRACT_MIN_PROTOCOL` | `22` | lowest supported network protocol |
| `ZENITH_CONTRACT_MAX_PROTOCOL` | `23` | highest verified network protocol |
| `ZENITH_SCHEDULED_UPGRADE` | unset | `<protocol>@<unix-seconds>` of an announced upgrade |
| `ZENITH_UPGRADE_DEPLOY_FREEZE_SECS` | `86400` | deploys freeze this long before an unsupported upgrade |
| `ZENITH_PROTOCOL_WEBHOOK_URL` | unset | receives every operator signal as JSON |

## Operator signals

Signals go through the `RunbookHook` trait. The default `LogHook` logs under
target `zenith::protocol`; `WebhookHook` POSTs the signal plus a `runbook`
link to `ZENITH_PROTOCOL_WEBHOOK_URL`. Alert on:

| Signal | Level | Action |
|---|---|---|
| `upgrade_scheduled` | info | follow [Before the upgrade](#before-the-upgrade) |
| `compatibility_degraded` | error | look up the [finding codes](#finding-codes) |
| `protocol_changed` | warn | follow [After the upgrade](#after-the-upgrade) |
| `compatibility_restored` | info | none |
| `rpc_unavailable` | error | see [RPC unavailable](#rpc-unavailable) |

## Before the upgrade

1. When SDF/validators announce the upgrade, set
   `ZENITH_SCHEDULED_UPGRADE=<protocol>@<unix-seconds>` and redeploy. The
   backend starts emitting `upgrade_scheduled` on every check.
2. If the new protocol is **outside** `ZENITH_CONTRACT_MAX_PROTOCOL`, deploys
   freeze `ZENITH_UPGRADE_DEPLOY_FREEZE_SECS` before the upgrade
   (`unsupported_upgrade_imminent`). Before it lands, either:
   * rebuild and test the contracts and backend against the new protocol
     (futurenet/testnet first), then raise `ZENITH_CONTRACT_MAX_PROTOCOL`; or
   * accept that chain mode stops at the upgrade until you do.
3. Make sure the RPC provider will upgrade their nodes at the same time.

## After the upgrade

1. `protocol_changed` fires on the first check after the network switches.
2. If the RPC hasn't been upgraded yet you will see
   `rpc_ledger_protocol_mismatch` and chain mode will not start — wait for
   (or switch to) an upgraded RPC.
3. Once the network is on the new protocol, clear `ZENITH_SCHEDULED_UPGRADE`.
4. Confirm in `protocol_observations` that the latest row is `compatible`.

## Finding codes

| Code | Severity | Meaning / fix |
|---|---|---|
| `network_passphrase_mismatch` | fatal | RPC URL points at the wrong network; fix `SOROBAN_RPC_URL` |
| `rpc_ledger_protocol_mismatch` | fatal | RPC build lags or leads the network; upgrade/switch RPC |
| `network_protocol_too_old` | fatal | contracts need a newer network; wrong network or misconfigured min |
| `network_protocol_unsupported` | fatal | network is past the verified SDK range; verify and raise max |
| `unsupported_upgrade_imminent` | blocks deploy | see [Before the upgrade](#before-the-upgrade) |
| `scheduled_upgrade_overdue` | info / blocks deploy | upgrade time passed but the network is still on the old protocol (vote failed or was delayed); update or clear `ZENITH_SCHEDULED_UPGRADE` |
| `upgrade_scheduled` | info | none |

## RPC unavailable

A check that can't reach the RPC clears the gate, so nothing new starts or
deploys until a check succeeds. Check the RPC's health; fail over to a
secondary provider if needed.
