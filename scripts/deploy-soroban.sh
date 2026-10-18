#!/usr/bin/env bash
set -euo pipefail

profile="${1:-local}"
descriptor="${2:-deploy/${profile}-contracts.json}"
rpc_url="${SOROBAN_RPC_URL:?SOROBAN_RPC_URL is required}"
network_passphrase="${STELLAR_NETWORK_PASSPHRASE:?STELLAR_NETWORK_PASSPHRASE is required}"
source_account="${STELLAR_DEPLOYER_ACCOUNT:?STELLAR_DEPLOYER_ACCOUNT must name a configured secret-backed Stellar identity}"

stellar rpc get-health --rpc-url "$rpc_url" >/dev/null
mkdir -p "$(dirname "$descriptor")"

deploy_contract() {
  local name="$1" wasm="$2"
  local existing
  existing="$(jq -r --arg name "$name" '.contracts[$name] // empty' "$descriptor" 2>/dev/null || true)"
  if [[ -n "$existing" ]]; then printf '%s' "$existing"; return; fi
  stellar contract deploy --wasm "$wasm" --source-account "$source_account" --rpc-url "$rpc_url" --network-passphrase "$network_passphrase"
}

options_id="$(deploy_contract options contracts/options.wasm)"
vault_id="$(deploy_contract vault contracts/vault.wasm)"
oracle_id="$(deploy_contract oracle contracts/oracle.wasm)"
jq -n --arg profile "$profile" --arg rpc "$rpc_url" --arg network "$network_passphrase" --arg options "$options_id" --arg vault "$vault_id" --arg oracle "$oracle_id" '{profile:$profile,rpc_url:$rpc,network_passphrase:$network,contracts:{options:$options,vault:$vault,oracle:$oracle}}' > "$descriptor"
