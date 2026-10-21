# Reproducible contract builds and hash pinning

Once the settlement contracts hold collateral, "the audited contract" and
"the deployed contract" must be the same claim, and anyone must be able to
check it. The chain of evidence is:

```
reviewed commit ──(pinned image, locked deps)──▶ WASM ──sha256──▶ pin ◀──sha256── on-chain WASM
```

Every arrow is checked mechanically:

| Check                                   | Where                                        |
|-----------------------------------------|----------------------------------------------|
| Pin manifest is well-formed             | `contracts.sh check-manifest`, CI on every PR |
| Two clean builds agree with each other and with the pin | `contracts.sh reproduce`, CI    |
| A local artifact matches its pin before deploy | `contracts.sh verify-artifact` / `deploy` |
| Deployed contracts run the pinned WASM  | `contracts.sh verify-deployed`, CI nightly   |
| Backend refuses to move funds through an unpinned or changed contract | `PinGuard` in `src/contract_pins.rs` |

A Soroban contract's WASM hash is the SHA-256 of the installed bytes, so a
locally computed hash and the network's hash compare directly.

## The manifest: `deploy/contract-pins.json`

```json
{
  "schema_version": 1,
  "contracts": [
    {
      "name": "zenith-vault",
      "network": "testnet",
      "contract_id": "C...",
      "wasm_sha256": "<64 lower-case hex>",
      "source_repo": "https://github.com/Zenith-options/contracts",
      "source_commit": "<full 40-char commit>",
      "build_image": "docker.io/<org>/soroban-build@sha256:<digest>",
      "audit_ref": "Audit report v1.2, section 3",
      "artifact": "zenith_vault.wasm"
    }
  ]
}
```

Rules, enforced by both the script and `PinManifest::validate`:

* `build_image` is pinned **by digest**. A tag such as `rust:1.89` can be
  re-pointed, so it does not pin anything.
* `source_commit` is a full commit hash, not a branch or tag.
* `contract_id` is absent until the contract is deployed. The backend
  ignores pins without one, and `PinGuard` refuses to route funds to any
  contract ID that has no pin.
* `artifact` defaults to `<name>.wasm` with `-` replaced by `_`, which is
  what Cargo names a `cdylib`.

The manifest ships empty. That is deliberate: until a reviewed pin exists,
nothing can be deployed or trusted.

## The build image

The image fixes the compiler, the standard library and every in-container
path. Build it once, push it, and pin the digest:

```dockerfile
FROM rust:1.89-slim
RUN rustup target add wasm32v1-none
```

The build refuses to run if the image lacks the target. Adding the target
at build time would make the image incomplete as a record of the build
environment. If you deploy optimized WASM (`stellar contract optimize`),
install the CLI in this image and run the optimizer as part of the build,
so the pinned hash is the optimized one.

The build runs `cargo build --locked --release --target wasm32v1-none` with:

* `--locked`: dependency versions come from the committed `Cargo.lock`.
* `SOURCE_DATE_EPOCH`: set from the commit time, not the clock.
* `RUSTFLAGS` cleared: host flags cannot leak into codegen.
* fixed paths (`/build`, `/tmp/cargo-home`): embedded paths are the same
  on every machine.

The contract crate's release profile should set `codegen-units = 1` and
`lto = true`. Both reduce nondeterminism, and the contract owns them, not
the build script.

## Releasing a contract

1. Merge the contract change in the contracts repo. Note the commit.
2. Build: `scripts/contracts.sh build <contracts-checkout> out/ <image@sha256:...>`.
3. Open a PR here that adds or updates the pin with the hash from
   `out/SHA256SUMS`, the commit and the image. CI's `reproduce` job
   rebuilds it twice from scratch. The PR cannot merge unless the rebuilds
   agree with each other and with the pin.
4. The audit or review references this exact hash in `audit_ref`.
5. Deploy only through the guarded path:
   `scripts/contracts.sh deploy zenith-vault testnet out/zenith_vault.wasm --source <key>`.
   It refuses a mismatched artifact, deploys, then fetches the deployed
   WASM back and checks it.
6. Record the printed `contract_id` in the manifest in a follow-up PR.

## Runtime guard

```rust
let manifest = PinManifest::load(Path::new(DEFAULT_MANIFEST_PATH))?;
let source = StellarCliHashSource::new("testnet");
let guard = PinGuard::verify(&manifest, "testnet", &source).await?; // refuse to start on error

// Immediately before moving funds through a contract:
guard.ensure_live(&vault_id, &source).await?;
```

`ensure_live` re-fetches the hash on every call because an in-place
upgrade would otherwise go unnoticed until the next restart. The cutover
(`src/cutover.rs`) and settlement execution (`src/settlement_review.rs`)
both call it before moving funds.

## Upgrades

A contract upgrade is a new pin. A pin holds one hash per `name` and
`network`, so there is always a window, between the pin change and the
on-chain upgrade, where the deployed hash and the pin disagree.
`ensure_live` fails closed during that window and value-moving calls stop.
That is the intended behaviour. Plan the window:

1. Get the new pin reproduced and reviewed in a PR. Do not merge yet.
2. Announce a maintenance window and drain the settlement queue.
3. Merge the pin and roll the backend so it loads the new manifest.
4. Submit the on-chain upgrade.
5. Run `scripts/contracts.sh verify-deployed <network>` and confirm the
   backend's `ensure_live` passes before reopening.
