# Chain Data Model

This document describes the persistent data model used by the Zenith Options backend. The canonical schema is defined in `migrations/postgres/0001_initial_schema.sql`.

## accounts

| Column | Type | Notes |
| --- | --- | --- |
| `wallet_address` | TEXT | Primary key |
| `balance` | NUMERIC | Defaults to `100000.0` |
| `collateral_locked` | NUMERIC | Defaults to `0.0` |
| `created_at` | TIMESTAMPTZ | Defaults to `NOW()` |

## positions

| Column | Type | Notes |
| --- | --- | --- |
| `id` | TEXT | Primary key |
| `wallet_address` | TEXT | References `accounts(wallet_address)` |
| `underlying` | TEXT | |
| `strike` | NUMERIC | |
| `expiry_days` | NUMERIC | |
| `option_type` | TEXT | `call` or `put` |
| `position_type` | TEXT | `long` or `short` |
| `contracts` | NUMERIC | |
| `entry_premium` | NUMERIC | |
| `entry_spot` | NUMERIC | |
| `collateral` | NUMERIC | Defaults to `0.0` |
| `status` | TEXT | `open`, `closed`, or `rolled`; defaults to `open` |
| `close_premium` | NUMERIC | Nullable |
| `close_spot` | NUMERIC | Nullable |
| `realized_pnl` | NUMERIC | Nullable |
| `opened_at` | TIMESTAMPTZ | Defaults to `NOW()` |
| `closed_at` | TIMESTAMPTZ | Nullable |
| `strategy_id` | TEXT | Nullable |

## watchlist

| Column | Type | Notes |
| --- | --- | --- |
| `wallet_address` | TEXT | References `accounts(wallet_address)`; part of composite primary key |
| `underlying` | TEXT | Part of composite primary key |
| `added_at` | TIMESTAMPTZ | Defaults to `NOW()` |

## alerts

| Column | Type | Notes |
| --- | --- | --- |
| `id` | TEXT | Primary key |
| `wallet_address` | TEXT | References `accounts(wallet_address)` |
| `underlying` | TEXT | |
| `target_price` | NUMERIC | |
| `direction` | TEXT | `above` or `below` |
| `status` | TEXT | `active`, `triggered`, or `dismissed`; defaults to `active` |
| `created_at` | TIMESTAMPTZ | Defaults to `NOW()` |
| `triggered_at` | TIMESTAMPTZ | Nullable |

## auth_nonces

| Column | Type | Notes |
| --- | --- | --- |
| `nonce` | TEXT | Primary key |
| `wallet_address` | TEXT | |
| `expires_at` | TIMESTAMPTZ | |
| `created_at` | TIMESTAMPTZ | Defaults to `NOW()` |

## sessions

| Column | Type | Notes |
| --- | --- | --- |
| `token` | TEXT | Primary key |
| `wallet_address` | TEXT | References `accounts(wallet_address)` |
| `expires_at` | TIMESTAMPTZ | |
| `created_at` | TIMESTAMPTZ | Defaults to `NOW()` |

## network_metadata

| Column | Type | Notes |
| --- | --- | --- |
| `id` | INTEGER | Primary key; constrained to `1` |
| `network` | TEXT | |
| `passphrase` | TEXT | |
| `created_at` | TIMESTAMPTZ | Defaults to `NOW()` |

## sponsored_transactions

| Column | Type | Notes |
| --- | --- | --- |
| `id` | TEXT | Primary key |
| `inner_tx_hash` | TEXT | Unique |
| `wallet_address` | TEXT | |
| `sponsor_account` | TEXT | |
| `fee_charged` | BIGINT | |
| `status` | TEXT | `submitted`, `confirmed`, or `failed`; defaults to `submitted` |
| `created_at` | TIMESTAMPTZ | Defaults to `NOW()` |

## contract_wasm_history

| Column | Type | Notes |
| --- | --- | --- |
| `contract_id` | TEXT | Part of composite primary key |
| `wasm_hash` | TEXT | Part of composite primary key |
| `from_ledger` | BIGINT | Part of composite primary key |
| `to_ledger` | BIGINT | Nullable |
| `created_at` | TIMESTAMPTZ | Defaults to `NOW()` |

## unprocessed_events

| Column | Type | Notes |
| --- | --- | --- |
| `id` | TEXT | Primary key |
| `contract_id` | TEXT | |
| `wasm_hash` | TEXT | |
| `ledger` | BIGINT | |
| `topics` | TEXT | |
| `data` | TEXT | |
| `error_reason` | TEXT | |
| `created_at` | TIMESTAMPTZ | Defaults to `NOW()` |

## leases

| Column | Type | Notes |
| --- | --- | --- |
| `name` | TEXT | Primary key |
| `holder` | TEXT | |
| `expires_at` | BIGINT | |
| `acquired_at` | BIGINT | |
| `renewed_at` | BIGINT | |

## keeper_job_executions

| Column | Type | Notes |
| --- | --- | --- |
| `job_id` | TEXT | Primary key |
| `task_name` | TEXT | |
| `target_id` | TEXT | |
| `status` | TEXT | `pending`, `executing`, `completed`, `failed`, or `skipped` |
| `fee_spent` | BIGINT | Defaults to `0` |
| `error_message` | TEXT | Nullable |
| `due_at` | BIGINT | |
| `executed_at` | BIGINT | Nullable |
| `created_at` | TIMESTAMPTZ | Defaults to `NOW()` |
