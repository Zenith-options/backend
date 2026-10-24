# Zenith options Soroban contract

This standalone Soroban contract escrows a configured SEP-41 collateral token
(USDC in the intended deployment), issues seven-decimal option balances,
accepts an immutable expiry fixing from the configured oracle, and transfers
cash payouts on claim. Admin authorization is required for series creation
and minting; the separate oracle authorization is required to settle.

Series have an explicit maximum payout per whole option. The admin deposits
that full cap before minting, rounded up to the token's 7-decimal unit. At
expiry, each holder receives the lesser of intrinsic value and the cap,
multiplied by their exercised quantity. After every issued unit is claimed,
unused collateral is returned to the series issuer. An uncapped short call
cannot be fully collateralized with finite USDC, so uncapped call series are
not supported by this contract.

Amounts and quantities are signed integers scaled by `10^7`; no floating-point
arithmetic is used in the contract. The option balances are maintained by this
contract and can be transferred before settlement; they are not separate
SEP-41 token contracts.

Build and test with a Rust toolchain and `wasm32v1-none` target installed:

```sh
cargo test --manifest-path contracts/options/Cargo.toml
cargo build --manifest-path contracts/options/Cargo.toml --target wasm32v1-none --release
```

The contract has not been deployed or independently audited. Before production
use, deploy against the intended USDC contract and test network, review role
key custody and oracle operations, and obtain an independent Soroban security
review.
