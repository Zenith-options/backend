#!/usr/bin/env bash
# Reproducible Soroban contract builds and hash pinning.
#
# The pinned hashes live in deploy/contract-pins.json (override with
# ZENITH_CONTRACT_PINS). See docs/contract-builds.md for the full procedure.
#
#   contracts.sh check-manifest
#       Validate the manifest's structure (digest-pinned images, full
#       commits, well-formed hashes, no duplicates).
#
#   contracts.sh build <source-dir> <out-dir> <build-image>
#       Build every contract in <source-dir> inside <build-image> (which must
#       be pinned by digest) and write the WASM plus SHA256SUMS to <out-dir>.
#
#   contracts.sh reproduce <network>
#       For each pin on <network>: clone source_repo at source_commit twice,
#       build each clone from scratch in the pinned image, and require both
#       builds and the pin to agree byte-for-byte.
#
#   contracts.sh verify-artifact <name> <network> <wasm-file>
#       Refuse a local WASM whose hash is not the pinned one.
#
#   contracts.sh verify-deployed <network>
#       Fetch every deployed pinned contract with the Stellar CLI and compare
#       its on-chain WASM hash with the pin.
#
#   contracts.sh deploy <name> <network> <wasm-file> [stellar deploy args...]
#       verify-artifact, then `stellar contract deploy`, then confirm the
#       freshly deployed contract runs the pinned hash.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST="${ZENITH_CONTRACT_PINS:-$ROOT/deploy/contract-pins.json}"
WASM_TARGET="${ZENITH_WASM_TARGET:-wasm32v1-none}"
STELLAR="${STELLAR_CLI:-stellar}"

die() { echo "error: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "$1 is required"; }
sha256() { sha256sum "$1" | cut -d' ' -f1; }

need jq
need sha256sum

# Prints the manifest problems, one per line. Mirrors PinManifest::validate
# in src/contract_pins.rs so CI catches a bad pin without building Rust.
manifest_problems() {
  jq -r '
    def hex($n): type == "string" and test("^[0-9a-f]{\($n)}$");
    (if .schema_version != 1 then "schema_version must be 1" else empty end),
    (.contracts // [] | group_by([.name, .network])[] | select(length > 1)
      | "\(.[0].name)@\(.[0].network): duplicate pin"),
    (.contracts // [] | map(select(.contract_id)) | group_by([.contract_id, .network])[]
      | select(length > 1) | "\(.[0].contract_id): pinned twice on \(.[0].network)"),
    (.contracts // [] | .[] | "\(.name)@\(.network)" as $at |
      (if (.name // "") == "" then "contract with empty name" else empty end),
      (if (.network | IN("testnet","mainnet","futurenet","standalone")) then empty else "\($at): unknown network" end),
      (if (.wasm_sha256 | hex(64)) then empty else "\($at): wasm_sha256 must be 64 lower-case hex characters" end),
      (if (.source_commit | hex(40)) then empty else "\($at): source_commit must be a full 40-character commit hash" end),
      (if (.source_repo // "") == "" then "\($at): source_repo is required" else empty end),
      (if ((.build_image // "") | test("^[^@]+@sha256:[0-9a-f]{64}$")) then empty
       else "\($at): build_image must be pinned by digest (image@sha256:<64 hex>), not a tag" end),
      (if .artifact == null or (.artifact | test("^[^/]+\\.wasm$") and (contains("..") | not)) then empty
       else "\($at): artifact must be a bare *.wasm file name" end),
      (if .contract_id == null or (.contract_id | test("^C[A-Z0-9]{55}$")) then empty
       else "\($at): contract_id must be a 56-character C... strkey" end)
    )' "$MANIFEST"
}

check_manifest() {
  [[ -f "$MANIFEST" ]] || die "manifest not found: $MANIFEST"
  jq empty "$MANIFEST" 2>/dev/null || die "$MANIFEST is not valid JSON"
  local problems
  problems="$(manifest_problems)"
  if [[ -n "$problems" ]]; then
    echo "$problems" >&2
    die "contract pin manifest is invalid"
  fi
  echo "manifest ok: $(jq '.contracts | length' "$MANIFEST") pin(s) in $MANIFEST"
}

pin_field() { # <name> <network> <field>
  jq -r --arg n "$1" --arg net "$2" --arg f "$3" \
    '.contracts[] | select(.name == $n and .network == $net) | .[$f] // empty' "$MANIFEST"
}

artifact_name() { # <name> <network>
  local a
  a="$(pin_field "$1" "$2" artifact)"
  if [[ -n "$a" ]]; then echo "$a"; else echo "${1//-/_}.wasm"; fi
}

build() { # <source-dir> <out-dir> <image>
  local src="$1" out="$2" image="$3"
  [[ "$image" =~ ^[^@]+@sha256:[0-9a-f]{64}$ ]] || die "build image must be pinned by digest: $image"
  [[ -f "$src/Cargo.lock" ]] || die "$src has no Cargo.lock; a reproducible build needs locked dependencies"
  need docker
  mkdir -p "$out"
  out="$(cd "$out" && pwd)"

  # Commit time, not wall-clock time, so embedded timestamps are stable.
  local epoch=0
  if git -C "$src" rev-parse HEAD >/dev/null 2>&1; then
    epoch="$(git -C "$src" log -1 --format=%ct)"
  fi

  # Fixed in-container paths (/build, /tmp/cargo-home) keep any embedded
  # paths identical across machines. RUSTFLAGS is cleared so the host
  # environment cannot leak into codegen.
  docker run --rm \
    --user "$(id -u):$(id -g)" \
    -v "$(cd "$src" && pwd)":/build \
    -w /build \
    -e HOME=/tmp \
    -e CARGO_HOME=/tmp/cargo-home \
    -e RUSTUP_HOME=/usr/local/rustup \
    -e CARGO_INCREMENTAL=0 \
    -e RUSTFLAGS= \
    -e SOURCE_DATE_EPOCH="$epoch" \
    -e TARGET="$WASM_TARGET" \
    "$image" \
    sh -euc 'rustup target list --installed | grep -qx "$TARGET" \
               || { echo "image lacks the $TARGET target; bake it into the pinned image" >&2; exit 1; }
             cargo build --locked --release --target "$TARGET"'

  local wasm_dir="$src/target/$WASM_TARGET/release"
  compgen -G "$wasm_dir/*.wasm" >/dev/null || die "build produced no WASM in $wasm_dir"
  cp "$wasm_dir"/*.wasm "$out/"
  (cd "$out" && sha256sum ./*.wasm | sed 's#\./##' > SHA256SUMS)
  cat "$out/SHA256SUMS"
}

reproduce() { # <network>
  local network="$1" work status=0
  check_manifest >/dev/null
  work="$(mktemp -d)"
  trap 'rm -rf "$work"' RETURN

  local count
  count="$(jq --arg net "$network" '[.contracts[] | select(.network == $net)] | length' "$MANIFEST")"
  if [[ "$count" == 0 ]]; then
    echo "no pins on $network; nothing to reproduce"
    return 0
  fi

  # Group by (repo, commit, image) so a workspace with several contracts is
  # built once per pass.
  while IFS=$'\t' read -r repo commit image; do
    local key i
    key="$(echo "$repo $commit $image" | sha256sum | cut -c1-12)"
    for i in 1 2; do
      git clone --quiet "$repo" "$work/$key-src$i"
      git -C "$work/$key-src$i" checkout --quiet --detach "$commit"
      echo "== build $i of $repo@$commit"
      build "$work/$key-src$i" "$work/$key-out$i" "$image" >/dev/null
    done
    if ! diff -u "$work/$key-out1/SHA256SUMS" "$work/$key-out2/SHA256SUMS"; then
      echo "NOT REPRODUCIBLE: two clean builds of $repo@$commit differ" >&2
      status=1
    fi

    while IFS=$'\t' read -r name expected; do
      local artifact actual
      artifact="$(artifact_name "$name" "$network")"
      if [[ ! -f "$work/$key-out1/$artifact" ]]; then
        echo "MISSING: $name ($artifact) not produced by $repo@$commit" >&2
        status=1; continue
      fi
      actual="$(sha256 "$work/$key-out1/$artifact")"
      if [[ "$actual" == "$expected" ]]; then
        echo "ok: $name@$network $actual"
      else
        echo "MISMATCH: $name@$network pinned $expected, rebuilt $actual" >&2
        status=1
      fi
    done < <(jq -r --arg net "$network" --arg r "$repo" --arg c "$commit" --arg i "$image" \
      '.contracts[] | select(.network == $net and .source_repo == $r and .source_commit == $c and .build_image == $i)
       | [.name, .wasm_sha256] | @tsv' "$MANIFEST")
  done < <(jq -r --arg net "$network" \
    '[.contracts[] | select(.network == $net) | [.source_repo, .source_commit, .build_image]] | unique[] | @tsv' \
    "$MANIFEST")
  return "$status"
}

verify_artifact() { # <name> <network> <wasm>
  local name="$1" network="$2" wasm="$3" expected actual
  check_manifest >/dev/null
  [[ -f "$wasm" ]] || die "no such file: $wasm"
  expected="$(pin_field "$name" "$network" wasm_sha256)"
  [[ -n "$expected" ]] || die "$name is not pinned on $network; add a reviewed pin before deploying"
  actual="$(sha256 "$wasm")"
  if [[ "$actual" != "$expected" ]]; then
    die "$wasm hash $actual does not match the pin for $name@$network ($expected); refusing"
  fi
  echo "ok: $wasm matches pin for $name@$network ($actual)"
}

deployed_hash() { # <contract-id> <network>
  local tmp
  tmp="$(mktemp)"
  "$STELLAR" contract fetch --id "$1" --network "$2" --out-file "$tmp" >/dev/null
  sha256 "$tmp"
  rm -f "$tmp"
}

verify_deployed() { # <network>
  local network="$1" status=0 n=0
  check_manifest >/dev/null
  need "$STELLAR"
  while IFS=$'\t' read -r name id expected; do
    n=$((n + 1))
    local actual
    if ! actual="$(deployed_hash "$id" "$network")"; then
      echo "ERROR: could not fetch $name ($id) on $network" >&2
      status=1; continue
    fi
    if [[ "$actual" == "$expected" ]]; then
      echo "ok: $name $id runs pinned $actual"
    else
      echo "MISMATCH: $name $id on $network runs $actual, pinned $expected" >&2
      status=1
    fi
  done < <(jq -r --arg net "$network" \
    '.contracts[] | select(.network == $net and .contract_id) | [.name, .contract_id, .wasm_sha256] | @tsv' \
    "$MANIFEST")
  echo "checked $n deployed contract(s) on $network"
  return "$status"
}

deploy() { # <name> <network> <wasm> [args...]
  local name="$1" network="$2" wasm="$3"
  shift 3
  verify_artifact "$name" "$network" "$wasm"
  need "$STELLAR"
  local id
  id="$("$STELLAR" contract deploy --wasm "$wasm" --network "$network" "$@" | tail -n1)"
  [[ "$id" =~ ^C[A-Z0-9]{55}$ ]] || die "unexpected deploy output: $id"
  local actual expected
  actual="$(deployed_hash "$id" "$network")"
  expected="$(pin_field "$name" "$network" wasm_sha256)"
  [[ "$actual" == "$expected" ]] || die "deployed $id runs $actual, expected $expected"
  echo "deployed $name to $id on $network, running pinned $actual"
  echo "next: record \"contract_id\": \"$id\" for $name@$network in $MANIFEST via a reviewed PR"
}

cmd="${1:-}"
shift || true
case "$cmd" in
  check-manifest) check_manifest ;;
  build) [[ $# -eq 3 ]] || die "usage: build <source-dir> <out-dir> <build-image>"; build "$@" ;;
  reproduce) [[ $# -eq 1 ]] || die "usage: reproduce <network>"; reproduce "$1" ;;
  verify-artifact) [[ $# -eq 3 ]] || die "usage: verify-artifact <name> <network> <wasm>"; verify_artifact "$@" ;;
  verify-deployed) [[ $# -eq 1 ]] || die "usage: verify-deployed <network>"; verify_deployed "$1" ;;
  deploy) [[ $# -ge 3 ]] || die "usage: deploy <name> <network> <wasm> [stellar args...]"; deploy "$@" ;;
  *) sed -n '2,32p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; [[ -z "$cmd" ]] || exit 2 ;;
esac
