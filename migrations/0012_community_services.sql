CREATE TABLE IF NOT EXISTS treasury_reports (id TEXT PRIMARY KEY, period_start INTEGER NOT NULL, period_end INTEGER NOT NULL, report_json TEXT NOT NULL, finalized_at INTEGER, UNIQUE(period_start, period_end));
CREATE TABLE IF NOT EXISTS voting_checkpoints (address TEXT NOT NULL, ledger INTEGER NOT NULL, balance TEXT NOT NULL, lock_end INTEGER, delegate TEXT, PRIMARY KEY(address, ledger));
CREATE TABLE IF NOT EXISTS voting_snapshots (id TEXT PRIMARY KEY, ledger INTEGER NOT NULL, merkle_root TEXT NOT NULL, entries_json TEXT NOT NULL, created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS protocol_params (key TEXT NOT NULL, value TEXT NOT NULL, effective_from INTEGER NOT NULL, source_proposal_id TEXT, set_by TEXT NOT NULL, PRIMARY KEY(key, effective_from));
CREATE TABLE IF NOT EXISTS referral_attributions (referee TEXT PRIMARY KEY, referrer TEXT NOT NULL, code TEXT NOT NULL, attributed_at INTEGER NOT NULL, CHECK(referee <> referrer));
CREATE TABLE IF NOT EXISTS reward_epochs (id TEXT PRIMARY KEY, starts_at INTEGER NOT NULL, ends_at INTEGER NOT NULL, merkle_root TEXT, finalized_at INTEGER);
CREATE TABLE IF NOT EXISTS reward_allocations (epoch_id TEXT NOT NULL, wallet TEXT NOT NULL, category TEXT NOT NULL, amount TEXT NOT NULL, justification TEXT, PRIMARY KEY(epoch_id, wallet, category));
