# Scoped session keys

Session keys are provisioned through a two-step, wallet-signed challenge. The
challenge binds the session public key, allowed `settlement`/`exercise`
operations, explicit series IDs, expiry, spend asset, spend ceiling, wallet,
and a one-time nonce. A wallet can only create, list, and revoke keys through
its authenticated backend session.

Use `POST /api/v1/session-keys/challenges` with an authenticated bearer token
and a JSON body containing `operations`, `series_ids`, `expires_in_seconds`,
`spend_asset`, and integer `spend_limit`. Sign the returned `challenge` string
with the authenticated wallet and submit its base64 Ed25519 signature with the
`challenge_id` to `POST /api/v1/session-keys`. `GET /api/v1/session-keys` lists
the wallet's grants; `DELETE /api/v1/session-keys/{id}` revokes one.

Set `ZENITH_SESSION_KEY_MASTER` to at least 32 random bytes through the
deployment secret manager. The backend derives a distinct Ed25519 key from this
secret and the random session-key ID; private key material is not stored in the
database or returned by the API. Rotating the master secret changes the derived
keys and invalidates existing grants, so revoke or let grants expire before
rotation.

The spend limit is an integer in the smallest unit of the declared asset.
Transaction builders must call the internal scoped signing function only after
constructing a trusted settlement/exercise transaction hash and supplying its
validated operation, series, asset, and spend amount. The spend reservation is
atomic and is intentionally not refunded when signing fails. Generic
user-supplied transaction payloads are not exposed as a signing endpoint.

The Soroban contract/account must also recognize the derived public key for
transaction authorization. The wallet-signed challenge establishes backend
consent and backend scope enforcement; it does not itself mutate Stellar
account signers or replace a contract's `__check_auth` policy.
