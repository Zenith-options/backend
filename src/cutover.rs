//! One-shot cutover from off-chain paper balances to on-chain Soroban escrow.
//!
//! Real users, positions and collateral already live in SQLite. Switching
//! settlement on-chain must leave no user stranded or silently deleted, and
//! must let the team prove that post-cutover balances equal pre-cutover
//! balances. Because the operation is irreversible on live data, it is
//! split into explicit, individually safe steps:
//!
//! 1. [`plan`]: a read-only dry run. It reports what would move and flags
//!    anything that would make the books not balance (negative balances,
//!    locked collateral that does not match open positions, positions
//!    without an account). Blocking issues prevent a snapshot.
//! 2. [`Cutover::snapshot`]: freezes the state into an immutable snapshot
//!    and marks every wallet `migrating`. Trading handlers must reject
//!    writes for wallets in that mode; see [`settlement_mode`].
//! 3. [`Cutover::migrate`]: screens each wallet (compliance), checks the
//!    escrow contract against its pinned hash, and credits escrow using
//!    idempotency keys. Progress is recorded per wallet, so a crash or a
//!    failed wallet is resumed by calling `migrate` again, never by
//!    re-crediting. Wallets that drifted since the snapshot, or that fail
//!    screening, are left off-chain (`failed` / `held`), not dropped.
//! 4. [`Cutover::verify`]: reconciles escrow against the snapshot exactly
//!    (integer stroops, zero tolerance), checks conservation
//!    (`snapshot = on-chain + held`), and confirms that every off-chain
//!    record still exists.
//! 5. [`Cutover::rollback`]: debits exactly what was credited and returns
//!    every wallet to `offchain`. The original `accounts`/`positions` rows
//!    are never modified by the cutover, so rollback makes them
//!    authoritative again with nothing to restore.
//!
//! [`rehearse`] runs steps 2–5 end-to-end against a copy of the database
//! (see [`copy_database`]) and an [`InMemoryEscrow`], which is how the
//! procedure is practised before it is run for real. The runbook is
//! `docs/runbooks/onchain-cutover.md`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{SqliteConnection, SqlitePool};

use crate::amounts::to_stroops;
use crate::compliance::{ComplianceError, ComplianceGate, Flow};
use crate::contract_pins::{PinError, PinGuard, WasmHashSource};

// ─── Escrow interface ─────────────────────────────────────────────────────────

/// Which escrow balance an amount belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bucket {
    /// Free cash balance.
    Free,
    /// Collateral locked against open short positions.
    Collateral,
}

impl Bucket {
    fn as_str(self) -> &'static str {
        match self {
            Bucket::Free => "free",
            Bucket::Collateral => "collateral",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct EscrowBalance {
    pub free: i64,
    pub collateral: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscrowError(pub String);

impl std::fmt::Display for EscrowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "escrow error: {}", self.0)
    }
}

/// The on-chain escrow the cutover credits. The production implementation
/// wraps the settlement contract's admin credit/debit entry points.
///
/// `credit` and `debit` **must be idempotent on `key`**. Replaying a key
/// returns the original transaction reference and moves nothing. This is
/// what makes `migrate` and `rollback` safe to re-run after a crash.
#[async_trait]
pub trait Escrow: Send + Sync {
    fn contract_id(&self) -> &str;
    async fn credit(
        &self,
        wallet: &str,
        bucket: Bucket,
        amount: i64,
        key: &str,
    ) -> Result<String, EscrowError>;
    async fn debit(
        &self,
        wallet: &str,
        bucket: Bucket,
        amount: i64,
        key: &str,
    ) -> Result<String, EscrowError>;
    async fn balance(&self, wallet: &str) -> Result<EscrowBalance, EscrowError>;
}

/// In-memory escrow for rehearsals and tests. Honours idempotency keys and
/// can be told to fail for specific wallets.
#[derive(Default)]
pub struct InMemoryEscrow {
    contract_id: String,
    state: Mutex<InMemoryState>,
}

#[derive(Default)]
struct InMemoryState {
    balances: HashMap<String, EscrowBalance>,
    applied: HashMap<String, String>,
    fail_wallets: HashSet<String>,
    tx_seq: u64,
}

impl InMemoryEscrow {
    pub fn new(contract_id: impl Into<String>) -> Self {
        Self {
            contract_id: contract_id.into(),
            state: Mutex::default(),
        }
    }

    pub fn fail_for(&self, wallet: &str, fail: bool) {
        let mut s = self.state.lock().unwrap();
        if fail {
            s.fail_wallets.insert(wallet.to_string());
        } else {
            s.fail_wallets.remove(wallet);
        }
    }

    /// Directly sets a balance, simulating an out-of-band change.
    pub fn force_balance(&self, wallet: &str, balance: EscrowBalance) {
        self.state
            .lock()
            .unwrap()
            .balances
            .insert(wallet.into(), balance);
    }

    pub fn total(&self) -> EscrowBalance {
        let s = self.state.lock().unwrap();
        s.balances
            .values()
            .fold(EscrowBalance::default(), |acc, b| EscrowBalance {
                free: acc.free + b.free,
                collateral: acc.collateral + b.collateral,
            })
    }

    fn apply(
        &self,
        wallet: &str,
        bucket: Bucket,
        delta: i64,
        key: &str,
    ) -> Result<String, EscrowError> {
        let mut s = self.state.lock().unwrap();
        if let Some(tx) = s.applied.get(key) {
            return Ok(tx.clone());
        }
        if s.fail_wallets.contains(wallet) {
            return Err(EscrowError(format!("injected failure for {wallet}")));
        }
        let entry = s.balances.entry(wallet.to_string()).or_default();
        let slot = match bucket {
            Bucket::Free => &mut entry.free,
            Bucket::Collateral => &mut entry.collateral,
        };
        if *slot + delta < 0 {
            return Err(EscrowError(format!(
                "insufficient {} balance for {wallet}",
                bucket.as_str()
            )));
        }
        *slot += delta;
        s.tx_seq += 1;
        let tx = format!("mem-tx-{}", s.tx_seq);
        s.applied.insert(key.to_string(), tx.clone());
        Ok(tx)
    }
}

#[async_trait]
impl Escrow for InMemoryEscrow {
    fn contract_id(&self) -> &str {
        &self.contract_id
    }
    async fn credit(
        &self,
        wallet: &str,
        bucket: Bucket,
        amount: i64,
        key: &str,
    ) -> Result<String, EscrowError> {
        self.apply(wallet, bucket, amount, key)
    }
    async fn debit(
        &self,
        wallet: &str,
        bucket: Bucket,
        amount: i64,
        key: &str,
    ) -> Result<String, EscrowError> {
        self.apply(wallet, bucket, -amount, key)
    }
    async fn balance(&self, wallet: &str) -> Result<EscrowBalance, EscrowError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .balances
            .get(wallet)
            .copied()
            .unwrap_or_default())
    }
}

// ─── Errors ───────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum CutoverError {
    Db(sqlx::Error),
    /// The plan has blocking issues; fix the data before snapshotting.
    Blocked(Vec<String>),
    /// Another cutover run is still in flight.
    RunInFlight,
    NotFound(String),
    InvalidState(String),
    Pins(PinError),
    Compliance(String),
}

impl std::fmt::Display for CutoverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(e) => write!(f, "database error: {e}"),
            Self::Blocked(issues) => write!(f, "cutover blocked: {}", issues.join("; ")),
            Self::RunInFlight => write!(f, "another cutover run is already in flight"),
            Self::NotFound(m) => write!(f, "not found: {m}"),
            Self::InvalidState(m) => write!(f, "invalid state: {m}"),
            Self::Pins(e) => write!(f, "escrow contract failed pin verification: {e}"),
            Self::Compliance(m) => write!(f, "compliance gate error: {m}"),
        }
    }
}

impl std::error::Error for CutoverError {}

impl From<sqlx::Error> for CutoverError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

// ─── Planning (read-only) ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct CutoverOptions {
    /// Refuse to cut over wallets that still have open positions. Set this
    /// when cutting over at an expiry boundary.
    pub require_flat: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Issue {
    pub blocking: bool,
    pub detail: String,
}

/// One wallet's off-chain state, in stroops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WalletState {
    pub wallet: String,
    pub free: i64,
    pub collateral: i64,
    pub open_positions: i64,
    pub open_collateral: i64,
    pub positions_sha256: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WalletPlan {
    #[serde(flatten)]
    pub state: WalletState,
    pub issues: Vec<Issue>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CutoverPlan {
    pub wallets: Vec<WalletPlan>,
    /// Wallets skipped because an earlier run already moved them on-chain.
    pub already_onchain: usize,
    pub total_free: i64,
    pub total_collateral: i64,
    pub total_open_positions: i64,
    /// SHA-256 over every wallet's state; identical plans have identical digests.
    pub sha256: String,
}

impl CutoverPlan {
    pub fn blocking_issues(&self) -> Vec<String> {
        self.wallets
            .iter()
            .flat_map(|w| {
                w.issues
                    .iter()
                    .filter(|i| i.blocking)
                    .map(move |i| format!("{}: {}", w.state.wallet, i.detail))
            })
            .collect()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Reads the live state of every wallet (or one wallet) that is not already
/// on-chain. Amount conversion problems become blocking issues, not errors,
/// so the dry run reports every problem at once.
async fn load_wallets(
    conn: &mut SqliteConnection,
    only: Option<&str>,
    options: &CutoverOptions,
) -> Result<(Vec<WalletPlan>, usize), CutoverError> {
    let onchain: HashSet<String> = sqlx::query_scalar(
        "SELECT wallet_address FROM account_settlement_mode WHERE mode = 'onchain'",
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();

    let accounts: Vec<(String, f64, f64)> = sqlx::query_as(
        "SELECT wallet_address, CAST(balance AS REAL), CAST(collateral_locked AS REAL) \
         FROM accounts WHERE (?1 IS NULL OR wallet_address = ?1) ORDER BY wallet_address",
    )
    .bind(only)
    .fetch_all(&mut *conn)
    .await?;

    type PositionRow = (String, String, String, f64, String, String, f64, f64);
    let positions: Vec<PositionRow> = sqlx::query_as(
        "SELECT id, wallet_address, underlying, CAST(strike AS REAL), option_type, position_type, \
                CAST(contracts AS REAL), CAST(collateral AS REAL) \
         FROM positions WHERE status = 'open' AND (?1 IS NULL OR wallet_address = ?1) \
         ORDER BY wallet_address, id",
    )
    .bind(only)
    .fetch_all(&mut *conn)
    .await?;

    struct Acc {
        state: WalletState,
        issues: Vec<Issue>,
        hasher: Sha256,
        has_account: bool,
    }
    let blank = |wallet: &str| Acc {
        state: WalletState {
            wallet: wallet.to_string(),
            free: 0,
            collateral: 0,
            open_positions: 0,
            open_collateral: 0,
            positions_sha256: String::new(),
        },
        issues: Vec::new(),
        hasher: Sha256::new(),
        has_account: false,
    };
    let convert = |issues: &mut Vec<Issue>, what: &str, v: f64| -> i64 {
        to_stroops(v).unwrap_or_else(|e| {
            issues.push(Issue {
                blocking: true,
                detail: format!("{what}: {e}"),
            });
            0
        })
    };

    let mut wallets: BTreeMap<String, Acc> = BTreeMap::new();
    let mut skipped = HashSet::new();
    for (wallet, balance, locked) in accounts {
        if onchain.contains(&wallet) {
            skipped.insert(wallet);
            continue;
        }
        let mut acc = blank(&wallet);
        acc.has_account = true;
        acc.state.free = convert(&mut acc.issues, "balance", balance);
        acc.state.collateral = convert(&mut acc.issues, "collateral_locked", locked);
        wallets.insert(wallet, acc);
    }
    for (id, wallet, underlying, strike, option_type, side, contracts, collateral) in positions {
        if onchain.contains(&wallet) {
            continue;
        }
        let acc = wallets
            .entry(wallet.clone())
            .or_insert_with(|| blank(&wallet));
        let strike = convert(&mut acc.issues, &format!("position {id} strike"), strike);
        let contracts = convert(
            &mut acc.issues,
            &format!("position {id} contracts"),
            contracts,
        );
        let collateral = convert(
            &mut acc.issues,
            &format!("position {id} collateral"),
            collateral,
        );
        acc.state.open_positions += 1;
        acc.state.open_collateral += collateral;
        acc.hasher.update(
            format!("{id}|{underlying}|{strike}|{option_type}|{side}|{contracts}|{collateral}\n")
                .as_bytes(),
        );
    }

    let mut plans = Vec::with_capacity(wallets.len());
    for (_, mut acc) in wallets {
        let s = &mut acc.state;
        s.positions_sha256 = hex(&acc.hasher.finalize());
        if !acc.has_account {
            acc.issues.push(Issue {
                blocking: true,
                detail: "open positions but no account row".into(),
            });
        }
        if s.free < 0 {
            acc.issues.push(Issue {
                blocking: true,
                detail: format!("negative free balance {}", s.free),
            });
        }
        if s.collateral < 0 {
            acc.issues.push(Issue {
                blocking: true,
                detail: format!("negative locked collateral {}", s.collateral),
            });
        }
        // Each position's collateral is rounded to stroops separately, so
        // allow one stroop of rounding per position and nothing more.
        if (s.collateral - s.open_collateral).abs() > s.open_positions {
            acc.issues.push(Issue {
                blocking: true,
                detail: format!(
                    "locked collateral {} does not match open positions' collateral {}",
                    s.collateral, s.open_collateral
                ),
            });
        }
        if options.require_flat && s.open_positions > 0 {
            acc.issues.push(Issue {
                blocking: true,
                detail: format!(
                    "{} open position(s); cutover requires a flat book",
                    s.open_positions
                ),
            });
        }
        if s.free == 0 && s.collateral == 0 && s.open_positions == 0 {
            acc.issues.push(Issue {
                blocking: false,
                detail: "empty account; will be marked on-chain without a transfer".into(),
            });
        }
        plans.push(WalletPlan {
            state: acc.state,
            issues: acc.issues,
        });
    }
    Ok((plans, skipped.len()))
}

fn plan_digest(wallets: &[WalletPlan]) -> String {
    let mut h = Sha256::new();
    for w in wallets {
        let s = &w.state;
        h.update(
            format!(
                "{}|{}|{}|{}|{}|{}\n",
                s.wallet,
                s.free,
                s.collateral,
                s.open_positions,
                s.open_collateral,
                s.positions_sha256
            )
            .as_bytes(),
        );
    }
    hex(&h.finalize())
}

async fn plan_on(
    conn: &mut SqliteConnection,
    options: &CutoverOptions,
) -> Result<CutoverPlan, CutoverError> {
    let (wallets, already_onchain) = load_wallets(conn, None, options).await?;
    Ok(CutoverPlan {
        total_free: wallets.iter().map(|w| w.state.free).sum(),
        total_collateral: wallets.iter().map(|w| w.state.collateral).sum(),
        total_open_positions: wallets.iter().map(|w| w.state.open_positions).sum(),
        sha256: plan_digest(&wallets),
        already_onchain,
        wallets,
    })
}

/// Dry run: what a cutover would move right now. Writes nothing.
pub async fn plan(
    pool: &SqlitePool,
    options: &CutoverOptions,
) -> Result<CutoverPlan, CutoverError> {
    let mut conn = pool.acquire().await?;
    plan_on(&mut conn, options).await
}

/// Where a wallet's funds currently live: `offchain` (default),
/// `migrating` (frozen; reject trading writes) or `onchain`.
pub async fn settlement_mode(pool: &SqlitePool, wallet: &str) -> Result<String, sqlx::Error> {
    Ok(
        sqlx::query_scalar("SELECT mode FROM account_settlement_mode WHERE wallet_address = ?")
            .bind(wallet)
            .fetch_optional(pool)
            .await?
            .unwrap_or_else(|| "offchain".to_string()),
    )
}

/// Copies the database to `dest` with `VACUUM INTO`, for rehearsals. The
/// copy is transactionally consistent even while the source is live.
pub async fn copy_database(pool: &SqlitePool, dest: &std::path::Path) -> Result<(), CutoverError> {
    if dest.exists() {
        return Err(CutoverError::InvalidState(format!(
            "{} already exists",
            dest.display()
        )));
    }
    sqlx::query("VACUUM INTO ?")
        .bind(dest.to_string_lossy().to_string())
        .execute(pool)
        .await?;
    // VACUUM INTO is a silent no-op for an in-memory source; never report
    // a rehearsal copy that does not exist.
    if !dest.exists() {
        return Err(CutoverError::InvalidState(format!(
            "VACUUM INTO did not write {}; is the source database in-memory?",
            dest.display()
        )));
    }
    Ok(())
}

// ─── Execution ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Run {
    pub id: String,
    pub label: String,
    pub status: String,
    pub contract_id: String,
    pub snapshot_sha256: String,
    pub wallet_count: i64,
    pub total_free: i64,
    pub total_collateral: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MigrateReport {
    pub migrated: usize,
    pub held: usize,
    pub failed: usize,
    /// True when every wallet has reached `migrated` or `held`.
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Mismatch {
    pub wallet: String,
    pub status: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReconciliationReport {
    pub run_id: String,
    pub wallets_checked: usize,
    pub migrated: usize,
    pub held: usize,
    pub snapshot_total: EscrowBalance,
    pub onchain_total: EscrowBalance,
    pub held_total: EscrowBalance,
    /// `snapshot_total == onchain_total + held_total`, exactly.
    pub conserved: bool,
    pub mismatches: Vec<Mismatch>,
}

impl ReconciliationReport {
    pub fn is_clean(&self) -> bool {
        self.conserved && self.mismatches.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RollbackReport {
    pub reverted: usize,
    pub failed: Vec<Mismatch>,
    pub complete: bool,
}

/// Everything the value-moving steps need.
pub struct Cutover<'a> {
    pub pool: &'a SqlitePool,
    pub escrow: &'a dyn Escrow,
    pub compliance: &'a ComplianceGate,
    pub pins: &'a PinGuard,
    pub hashes: &'a dyn WasmHashSource,
}

type SnapshotRow = (String, i64, i64, i64, i64, String);
type RunRow = (String, String, String, String, String, i64, i64, i64);

impl<'a> Cutover<'a> {
    async fn event(
        &self,
        run_id: &str,
        wallet: Option<&str>,
        event: &str,
        actor: &str,
        detail: &str,
        now: i64,
    ) -> Result<(), CutoverError> {
        sqlx::query(
            "INSERT INTO cutover_events (run_id, wallet_address, event, actor, detail, created_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(run_id)
        .bind(wallet)
        .bind(event)
        .bind(actor)
        .bind(detail)
        .bind(now)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn run(&self, run_id: &str) -> Result<Run, CutoverError> {
        let row: Option<RunRow> = sqlx::query_as(
            "SELECT id, label, status, contract_id, snapshot_sha256, wallet_count, total_free, total_collateral \
             FROM cutover_runs WHERE id = ?",
        )
        .bind(run_id)
        .fetch_optional(self.pool)
        .await?;
        let (
            id,
            label,
            status,
            contract_id,
            snapshot_sha256,
            wallet_count,
            total_free,
            total_collateral,
        ) = row.ok_or_else(|| CutoverError::NotFound(format!("cutover run {run_id}")))?;
        Ok(Run {
            id,
            label,
            status,
            contract_id,
            snapshot_sha256,
            wallet_count,
            total_free,
            total_collateral,
        })
    }

    async fn set_run_status(
        &self,
        run_id: &str,
        status: &str,
        now: i64,
    ) -> Result<(), CutoverError> {
        sqlx::query("UPDATE cutover_runs SET status = ?, updated_at = ? WHERE id = ?")
            .bind(status)
            .bind(now)
            .bind(run_id)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Freezes the current state into an immutable snapshot and marks every
    /// wallet in it `migrating`, all in one transaction. Refuses if the plan
    /// has blocking issues or another run is in flight.
    pub async fn snapshot(
        &self,
        label: &str,
        options: &CutoverOptions,
        actor: &str,
        now: i64,
    ) -> Result<Run, CutoverError> {
        let mut tx = self.pool.begin().await?;
        let plan = plan_on(&mut tx, options).await?;
        let blocking = plan.blocking_issues();
        if !blocking.is_empty() {
            return Err(CutoverError::Blocked(blocking));
        }
        let run_id = format!("cutover-{now}-{}", &plan.sha256[..12]);
        let inserted = sqlx::query(
            "INSERT INTO cutover_runs \
                 (id, label, status, contract_id, snapshot_sha256, wallet_count, total_free, total_collateral, \
                  created_by, created_at, updated_at) \
             VALUES (?, ?, 'snapshotted', ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&run_id)
        .bind(label)
        .bind(self.escrow.contract_id())
        .bind(&plan.sha256)
        .bind(plan.wallets.len() as i64)
        .bind(plan.total_free)
        .bind(plan.total_collateral)
        .bind(actor)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await;
        match inserted {
            Err(sqlx::Error::Database(e)) if e.message().contains("UNIQUE") => {
                return Err(CutoverError::RunInFlight)
            }
            other => {
                other?;
            }
        }
        for w in &plan.wallets {
            let s = &w.state;
            sqlx::query(
                "INSERT INTO cutover_snapshot_wallets \
                     (run_id, wallet_address, free_stroops, collateral_stroops, open_positions, \
                      open_collateral_stroops, positions_sha256) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&run_id)
            .bind(&s.wallet)
            .bind(s.free)
            .bind(s.collateral)
            .bind(s.open_positions)
            .bind(s.open_collateral)
            .bind(&s.positions_sha256)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO cutover_wallets (run_id, wallet_address, status, updated_at) VALUES (?, ?, 'pending', ?)",
            )
            .bind(&run_id)
            .bind(&s.wallet)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO account_settlement_mode (wallet_address, mode, run_id, updated_at) \
                 VALUES (?, 'migrating', ?, ?) \
                 ON CONFLICT (wallet_address) DO UPDATE SET mode = 'migrating', run_id = excluded.run_id, \
                     updated_at = excluded.updated_at",
            )
            .bind(&s.wallet)
            .bind(&run_id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "INSERT INTO cutover_events (run_id, event, actor, detail, created_at) VALUES (?, 'snapshotted', ?, ?, ?)",
        )
        .bind(&run_id)
        .bind(actor)
        .bind(format!(
            "{} wallets, free {}, collateral {}, sha256 {}",
            plan.wallets.len(),
            plan.total_free,
            plan.total_collateral,
            plan.sha256
        ))
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.run(&run_id).await
    }

    async fn snapshot_row(&self, run_id: &str, wallet: &str) -> Result<SnapshotRow, CutoverError> {
        Ok(sqlx::query_as(
            "SELECT wallet_address, free_stroops, collateral_stroops, open_positions, open_collateral_stroops, \
                    positions_sha256 \
             FROM cutover_snapshot_wallets WHERE run_id = ? AND wallet_address = ?",
        )
        .bind(run_id)
        .bind(wallet)
        .fetch_one(self.pool)
        .await?)
    }

    async fn set_wallet(
        &self,
        run_id: &str,
        wallet: &str,
        status: &str,
        detail: &str,
        now: i64,
    ) -> Result<(), CutoverError> {
        sqlx::query(
            "UPDATE cutover_wallets SET status = ?, detail = ?, updated_at = ? WHERE run_id = ? AND wallet_address = ?",
        )
        .bind(status)
        .bind(detail)
        .bind(now)
        .bind(run_id)
        .bind(wallet)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    async fn set_mode(
        &self,
        wallet: &str,
        mode: &str,
        run_id: &str,
        now: i64,
    ) -> Result<(), CutoverError> {
        sqlx::query("UPDATE account_settlement_mode SET mode = ?, run_id = ?, updated_at = ? WHERE wallet_address = ?")
            .bind(mode)
            .bind(run_id)
            .bind(now)
            .bind(wallet)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Credits escrow for every `pending` or `failed` wallet. Safe to call
    /// repeatedly: completed credits are recorded and keyed, so a re-run
    /// resumes instead of double-crediting.
    pub async fn migrate(
        &self,
        run_id: &str,
        actor: &str,
        now: i64,
    ) -> Result<MigrateReport, CutoverError> {
        let run = self.run(run_id).await?;
        if !matches!(run.status.as_str(), "snapshotted" | "migrating") {
            return Err(CutoverError::InvalidState(format!(
                "run {run_id} is {}",
                run.status
            )));
        }
        if run.contract_id != self.escrow.contract_id() {
            return Err(CutoverError::InvalidState(format!(
                "run targets {} but escrow is {}",
                run.contract_id,
                self.escrow.contract_id()
            )));
        }
        // Never move funds into a contract that is not the reviewed one.
        self.pins
            .ensure_live(&run.contract_id, self.hashes)
            .await
            .map_err(CutoverError::Pins)?;
        self.set_run_status(run_id, "migrating", now).await?;
        self.event(run_id, None, "migrate_started", actor, "", now)
            .await?;

        let todo: Vec<(String, i64, i64)> = sqlx::query_as(
            "SELECT wallet_address, free_credited, collateral_credited FROM cutover_wallets \
             WHERE run_id = ? AND status IN ('pending', 'failed') ORDER BY wallet_address",
        )
        .bind(run_id)
        .fetch_all(self.pool)
        .await?;

        for (wallet, free_credited, collateral_credited) in todo {
            self.migrate_wallet(
                run_id,
                &wallet,
                free_credited,
                collateral_credited,
                actor,
                now,
            )
            .await?;
        }

        let counts: Vec<(String, i64)> = sqlx::query_as(
            "SELECT status, COUNT(*) FROM cutover_wallets WHERE run_id = ? GROUP BY status",
        )
        .bind(run_id)
        .fetch_all(self.pool)
        .await?;
        let count = |s: &str| {
            counts
                .iter()
                .find(|(k, _)| k == s)
                .map(|(_, n)| *n as usize)
                .unwrap_or(0)
        };
        let report = MigrateReport {
            migrated: count("migrated"),
            held: count("held"),
            failed: count("failed"),
            complete: count("pending") == 0 && count("failed") == 0,
        };
        if report.complete {
            self.set_run_status(run_id, "migrated", now).await?;
        }
        self.event(
            run_id,
            None,
            "migrate_finished",
            actor,
            &format!(
                "migrated {}, held {}, failed {}",
                report.migrated, report.held, report.failed
            ),
            now,
        )
        .await?;
        Ok(report)
    }

    async fn migrate_wallet(
        &self,
        run_id: &str,
        wallet: &str,
        mut free_credited: i64,
        mut collateral_credited: i64,
        actor: &str,
        now: i64,
    ) -> Result<(), CutoverError> {
        let (_, free, collateral, open_positions, open_collateral, positions_sha256) =
            self.snapshot_row(run_id, wallet).await?;

        // 1. Drift: the live off-chain state must still equal the snapshot.
        let mut conn = self.pool.acquire().await?;
        let (live, _) = load_wallets(&mut conn, Some(wallet), &CutoverOptions::default()).await?;
        drop(conn);
        let live = live.into_iter().next().map(|p| p.state);
        let expected = WalletState {
            wallet: wallet.to_string(),
            free,
            collateral,
            open_positions,
            open_collateral,
            positions_sha256,
        };
        if live.as_ref() != Some(&expected) {
            let detail = "off-chain state changed since snapshot; re-snapshot required";
            self.set_wallet(run_id, wallet, "failed", detail, now)
                .await?;
            self.event(run_id, Some(wallet), "drift", actor, detail, now)
                .await?;
            return Ok(());
        }

        // 2. Compliance: a wallet that fails screening stays off-chain.
        let decision = match self.compliance.check(wallet, Flow::Deposit, now).await {
            Ok(d) => d,
            Err(ComplianceError::Db(e)) => return Err(CutoverError::Db(e)),
            Err(e) => return Err(CutoverError::Compliance(e.to_string())),
        };
        if !decision.allowed && decision.enforced {
            // Only the decision id is recorded here; the reason stays in
            // the compliance audit table.
            let detail = format!(
                "compliance hold (decision #{})",
                decision.decision_id.unwrap_or_default()
            );
            self.set_wallet(run_id, wallet, "held", &detail, now)
                .await?;
            self.set_mode(wallet, "offchain", run_id, now).await?;
            self.event(run_id, Some(wallet), "held", actor, &detail, now)
                .await?;
            return Ok(());
        }

        // 3. Escrow must start empty for this wallet, unless we are resuming.
        if free_credited == 0 && collateral_credited == 0 {
            match self.escrow.balance(wallet).await {
                Ok(b) if b == EscrowBalance::default() => {}
                Ok(b) => {
                    let detail = format!("escrow not empty before credit: {b:?}");
                    self.set_wallet(run_id, wallet, "failed", &detail, now)
                        .await?;
                    self.event(run_id, Some(wallet), "failed", actor, &detail, now)
                        .await?;
                    return Ok(());
                }
                Err(e) => {
                    self.set_wallet(run_id, wallet, "failed", &e.to_string(), now)
                        .await?;
                    return Ok(());
                }
            }
        }

        // 4. Credit each bucket once, recording progress after each step.
        for (bucket, amount, credited) in [
            (Bucket::Free, free, &mut free_credited),
            (Bucket::Collateral, collateral, &mut collateral_credited),
        ] {
            if amount == 0 || *credited == amount {
                continue;
            }
            let key = format!("{run_id}:{wallet}:{}", bucket.as_str());
            match self.escrow.credit(wallet, bucket, amount, &key).await {
                Ok(tx) => {
                    let sql = match bucket {
                        Bucket::Free => "UPDATE cutover_wallets SET free_credited = ?, free_tx = ?, updated_at = ? WHERE run_id = ? AND wallet_address = ?",
                        Bucket::Collateral => "UPDATE cutover_wallets SET collateral_credited = ?, collateral_tx = ?, updated_at = ? WHERE run_id = ? AND wallet_address = ?",
                    };
                    sqlx::query(sql)
                        .bind(amount)
                        .bind(&tx)
                        .bind(now)
                        .bind(run_id)
                        .bind(wallet)
                        .execute(self.pool)
                        .await?;
                    *credited = amount;
                    self.event(
                        run_id,
                        Some(wallet),
                        "credited",
                        actor,
                        &format!("{} {amount} tx {tx}", bucket.as_str()),
                        now,
                    )
                    .await?;
                }
                Err(e) => {
                    self.set_wallet(run_id, wallet, "failed", &e.to_string(), now)
                        .await?;
                    self.event(run_id, Some(wallet), "failed", actor, &e.to_string(), now)
                        .await?;
                    return Ok(());
                }
            }
        }

        self.set_wallet(run_id, wallet, "migrated", "", now).await?;
        self.set_mode(wallet, "onchain", run_id, now).await?;
        self.event(run_id, Some(wallet), "migrated", actor, "", now)
            .await?;
        Ok(())
    }

    /// Reconciles escrow against the snapshot with zero tolerance. Marks the
    /// run `verified` when it is fully migrated and clean.
    pub async fn verify(
        &self,
        run_id: &str,
        actor: &str,
        now: i64,
    ) -> Result<ReconciliationReport, CutoverError> {
        let run = self.run(run_id).await?;
        let rows: Vec<(String, i64, i64, String, i64, i64)> = sqlx::query_as(
            "SELECT s.wallet_address, s.free_stroops, s.collateral_stroops, w.status, w.free_credited, \
                    w.collateral_credited \
             FROM cutover_snapshot_wallets s \
             JOIN cutover_wallets w ON w.run_id = s.run_id AND w.wallet_address = s.wallet_address \
             WHERE s.run_id = ? ORDER BY s.wallet_address",
        )
        .bind(run_id)
        .fetch_all(self.pool)
        .await?;
        let accounts: HashSet<String> = sqlx::query_scalar("SELECT wallet_address FROM accounts")
            .fetch_all(self.pool)
            .await?
            .into_iter()
            .collect();

        let mut report = ReconciliationReport {
            run_id: run_id.to_string(),
            wallets_checked: rows.len(),
            migrated: 0,
            held: 0,
            snapshot_total: EscrowBalance::default(),
            onchain_total: EscrowBalance::default(),
            held_total: EscrowBalance::default(),
            conserved: false,
            mismatches: Vec::new(),
        };
        let mut mismatch = |wallet: &str, status: &str, detail: String| {
            report.mismatches.push(Mismatch {
                wallet: wallet.into(),
                status: status.into(),
                detail,
            })
        };
        let mut snapshot_total = EscrowBalance::default();
        let mut onchain_total = EscrowBalance::default();
        let mut held_total = EscrowBalance::default();
        let (mut migrated, mut held) = (0, 0);

        for (wallet, free, collateral, status, free_credited, collateral_credited) in rows {
            snapshot_total.free += free;
            snapshot_total.collateral += collateral;
            if !accounts.contains(&wallet) {
                mismatch(
                    &wallet,
                    &status,
                    "off-chain account record is missing".into(),
                );
            }
            let actual = match self.escrow.balance(&wallet).await {
                Ok(b) => b,
                Err(e) => {
                    mismatch(&wallet, &status, e.to_string());
                    continue;
                }
            };
            let credited = EscrowBalance {
                free: free_credited,
                collateral: collateral_credited,
            };
            if actual != credited {
                mismatch(
                    &wallet,
                    &status,
                    format!("escrow holds {actual:?}, cutover credited {credited:?}"),
                );
            }
            match status.as_str() {
                "migrated" => {
                    migrated += 1;
                    onchain_total.free += actual.free;
                    onchain_total.collateral += actual.collateral;
                    if credited != (EscrowBalance { free, collateral }) {
                        mismatch(
                            &wallet,
                            &status,
                            format!("credited {credited:?}, snapshot {free}/{collateral}"),
                        );
                    }
                }
                "held" => {
                    held += 1;
                    held_total.free += free;
                    held_total.collateral += collateral;
                }
                other => mismatch(&wallet, other, "wallet has not finished migrating".into()),
            }
        }
        report.migrated = migrated;
        report.held = held;
        report.conserved = snapshot_total.free == onchain_total.free + held_total.free
            && snapshot_total.collateral == onchain_total.collateral + held_total.collateral;
        report.snapshot_total = snapshot_total;
        report.onchain_total = onchain_total;
        report.held_total = held_total;

        let clean = report.is_clean();
        if clean && run.status == "migrated" {
            self.set_run_status(run_id, "verified", now).await?;
        }
        self.event(
            run_id,
            None,
            if clean {
                "verify_clean"
            } else {
                "verify_mismatch"
            },
            actor,
            &serde_json::to_string(&report).unwrap_or_default(),
            now,
        )
        .await?;
        Ok(report)
    }

    /// Debits exactly what this run credited and returns every wallet to
    /// `offchain`. Idempotent: re-run it to finish after a partial failure.
    pub async fn rollback(
        &self,
        run_id: &str,
        actor: &str,
        reason: &str,
        now: i64,
    ) -> Result<RollbackReport, CutoverError> {
        let run = self.run(run_id).await?;
        if run.status == "rolled_back" {
            return Ok(RollbackReport {
                complete: true,
                ..RollbackReport::default()
            });
        }
        if reason.trim().is_empty() {
            return Err(CutoverError::InvalidState(
                "rollback requires a reason".into(),
            ));
        }
        self.event(run_id, None, "rollback_started", actor, reason, now)
            .await?;
        let rows: Vec<(String, String, i64, i64)> = sqlx::query_as(
            "SELECT wallet_address, status, free_credited, collateral_credited FROM cutover_wallets \
             WHERE run_id = ? AND status != 'reverted' ORDER BY wallet_address",
        )
        .bind(run_id)
        .fetch_all(self.pool)
        .await?;

        let mut report = RollbackReport::default();
        'wallets: for (wallet, status, free_credited, collateral_credited) in rows {
            for (bucket, amount) in [
                (Bucket::Collateral, collateral_credited),
                (Bucket::Free, free_credited),
            ] {
                if amount == 0 {
                    continue;
                }
                let key = format!("{run_id}:{wallet}:rollback:{}", bucket.as_str());
                match self.escrow.debit(&wallet, bucket, amount, &key).await {
                    Ok(tx) => {
                        let sql = match bucket {
                            Bucket::Free => "UPDATE cutover_wallets SET free_credited = 0, updated_at = ? WHERE run_id = ? AND wallet_address = ?",
                            Bucket::Collateral => "UPDATE cutover_wallets SET collateral_credited = 0, updated_at = ? WHERE run_id = ? AND wallet_address = ?",
                        };
                        sqlx::query(sql)
                            .bind(now)
                            .bind(run_id)
                            .bind(&wallet)
                            .execute(self.pool)
                            .await?;
                        self.event(
                            run_id,
                            Some(&wallet),
                            "debited",
                            actor,
                            &format!("{} {amount} tx {tx}", bucket.as_str()),
                            now,
                        )
                        .await?;
                    }
                    Err(e) => {
                        report.failed.push(Mismatch {
                            wallet: wallet.clone(),
                            status: status.clone(),
                            detail: e.to_string(),
                        });
                        self.event(
                            run_id,
                            Some(&wallet),
                            "rollback_failed",
                            actor,
                            &e.to_string(),
                            now,
                        )
                        .await?;
                        continue 'wallets;
                    }
                }
            }
            self.set_wallet(run_id, &wallet, "reverted", reason, now)
                .await?;
            self.set_mode(&wallet, "offchain", run_id, now).await?;
            report.reverted += 1;
        }
        report.complete = report.failed.is_empty();
        if report.complete {
            self.set_run_status(run_id, "rolled_back", now).await?;
            self.event(run_id, None, "rolled_back", actor, reason, now)
                .await?;
        }
        Ok(report)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RehearsalReport {
    pub run: Run,
    pub migrate: MigrateReport,
    pub reconciliation: ReconciliationReport,
    pub rollback: RollbackReport,
    /// Escrow is back to exactly zero for every snapshot wallet.
    pub rollback_restored_escrow: bool,
    /// Every wallet is back to `offchain`.
    pub rollback_restored_modes: bool,
}

impl RehearsalReport {
    pub fn passed(&self) -> bool {
        self.migrate.complete
            && self.reconciliation.is_clean()
            && self.rollback.complete
            && self.rollback_restored_escrow
            && self.rollback_restored_modes
    }
}

/// Full dress rehearsal: snapshot → migrate → verify → rollback, then
/// checks that rollback restored the starting point. Run this against a
/// [`copy_database`] copy with an [`InMemoryEscrow`], never production.
pub async fn rehearse(
    cutover: &Cutover<'_>,
    options: &CutoverOptions,
    actor: &str,
    now: i64,
) -> Result<RehearsalReport, CutoverError> {
    let run = cutover.snapshot("rehearsal", options, actor, now).await?;
    let migrate = cutover.migrate(&run.id, actor, now).await?;
    let reconciliation = cutover.verify(&run.id, actor, now).await?;
    let rollback = cutover.rollback(&run.id, actor, "rehearsal", now).await?;

    let wallets: Vec<String> =
        sqlx::query_scalar("SELECT wallet_address FROM cutover_snapshot_wallets WHERE run_id = ?")
            .bind(&run.id)
            .fetch_all(cutover.pool)
            .await?;
    let mut rollback_restored_escrow = true;
    for w in &wallets {
        if cutover.escrow.balance(w).await.ok() != Some(EscrowBalance::default()) {
            rollback_restored_escrow = false;
        }
    }
    let not_offchain: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM account_settlement_mode m JOIN cutover_snapshot_wallets s \
           ON s.wallet_address = m.wallet_address AND s.run_id = ? WHERE m.mode != 'offchain'",
    )
    .bind(&run.id)
    .fetch_one(cutover.pool)
    .await?;
    Ok(RehearsalReport {
        run: cutover.run(&run.id).await?,
        migrate,
        reconciliation,
        rollback,
        rollback_restored_escrow,
        rollback_restored_modes: not_offchain == 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compliance::{ComplianceMode, CompliancePolicy, DenylistProvider};
    use crate::contract_pins::{sha256_hex, ContractPin, PinManifest, StaticHashSource};
    use std::sync::Arc;

    const ESCROW_ID: &str = "CESCROWAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const ALICE: &str = "GALICE";
    const BOB: &str = "GBOB";
    const CAROL: &str = "GCAROL";
    const SANCTIONED: &str = "GSANCTIONED";
    const UNIT: i64 = crate::amounts::STROOPS_PER_UNIT;

    async fn pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        for sql in [
            include_str!("../migrations/0001_accounts.sql"),
            include_str!("../migrations/0002_positions.sql"),
            include_str!("../migrations/0012_compliance_screening.sql"),
            include_str!("../migrations/0013_onchain_cutover.sql"),
        ] {
            sqlx::raw_sql(sql).execute(&pool).await.unwrap();
        }
        // Alice: cash only. Bob: cash plus a short put backed by 1100 of
        // collateral. Carol: empty.
        sqlx::raw_sql(
            "INSERT INTO accounts (wallet_address, balance, collateral_locked) VALUES
                 ('GALICE', 100000.0, 0.0), ('GBOB', 2500.5, 1100.0), ('GCAROL', 0.0, 0.0);
             INSERT INTO positions (id, wallet_address, underlying, strike, expiry_days, option_type,
                                    position_type, contracts, entry_premium, entry_spot, collateral)
             VALUES ('p1', 'GBOB', 'XLM', 1000.0, 7, 'put', 'short', 1, 12.0, 1050.0, 1100.0),
                    ('p2', 'GALICE', 'BTC', 70000.0, 7, 'call', 'long', 1, 500.0, 67000.0, 0.0);",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    /// Writes the in-memory test database to a file by replaying its schema
    /// and rows, since VACUUM INTO cannot copy an in-memory source.
    async fn copy_database_for_test(src: &SqlitePool, dest: &std::path::Path) {
        let file = SqlitePool::connect(&format!("sqlite://{}?mode=rwc", dest.display()))
            .await
            .unwrap();
        let schema: Vec<String> = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' \
             ORDER BY CASE type WHEN 'table' THEN 0 ELSE 1 END",
        )
        .fetch_all(src)
        .await
        .unwrap();
        for sql in schema {
            sqlx::raw_sql(&sql).execute(&file).await.unwrap();
        }
        let accounts: Vec<(String, f64, f64)> =
            sqlx::query_as("SELECT wallet_address, balance, collateral_locked FROM accounts")
                .fetch_all(src)
                .await
                .unwrap();
        for (w, b, c) in accounts {
            sqlx::query("INSERT INTO accounts (wallet_address, balance, collateral_locked) VALUES (?, ?, ?)")
                .bind(w).bind(b).bind(c).execute(&file).await.unwrap();
        }
        type P = (
            String,
            String,
            String,
            f64,
            f64,
            String,
            String,
            f64,
            f64,
            f64,
            f64,
        );
        let positions: Vec<P> = sqlx::query_as(
            "SELECT id, wallet_address, underlying, strike, expiry_days, option_type, position_type, contracts, \
                    entry_premium, entry_spot, collateral FROM positions",
        )
        .fetch_all(src)
        .await
        .unwrap();
        for p in positions {
            sqlx::query(
                "INSERT INTO positions (id, wallet_address, underlying, strike, expiry_days, option_type, \
                     position_type, contracts, entry_premium, entry_spot, collateral) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(p.0).bind(p.1).bind(p.2).bind(p.3).bind(p.4).bind(p.5).bind(p.6).bind(p.7).bind(p.8).bind(p.9).bind(p.10)
            .execute(&file)
            .await
            .unwrap();
        }
        file.close().await;
    }

    struct Env {
        pool: SqlitePool,
        escrow: InMemoryEscrow,
        gate: ComplianceGate,
        pins: PinGuard,
        hashes: StaticHashSource,
    }

    impl Env {
        async fn new(mode: ComplianceMode) -> Self {
            let pool = pool().await;
            let gate = ComplianceGate::new(
                pool.clone(),
                Arc::new(DenylistProvider::new(
                    "t",
                    [(SANCTIONED.to_string(), "SDN".to_string())],
                )),
                CompliancePolicy {
                    mode,
                    ..CompliancePolicy::default()
                },
            );
            let manifest = PinManifest {
                schema_version: 1,
                contracts: vec![ContractPin {
                    name: "zenith-vault".into(),
                    network: "testnet".into(),
                    contract_id: Some(ESCROW_ID.into()),
                    wasm_sha256: sha256_hex(b"vault"),
                    source_repo: "r".into(),
                    source_commit: "a".repeat(40),
                    build_image: format!("img@sha256:{}", "0".repeat(64)),
                    audit_ref: None,
                    artifact: None,
                }],
            };
            let mut hashes = StaticHashSource::default();
            hashes.0.insert(ESCROW_ID.into(), sha256_hex(b"vault"));
            let pins = PinGuard::verify(&manifest, "testnet", &hashes)
                .await
                .unwrap();
            Self {
                pool,
                escrow: InMemoryEscrow::new(ESCROW_ID),
                gate,
                pins,
                hashes,
            }
        }

        fn cutover(&self) -> Cutover<'_> {
            Cutover {
                pool: &self.pool,
                escrow: &self.escrow,
                compliance: &self.gate,
                pins: &self.pins,
                hashes: &self.hashes,
            }
        }
    }

    #[tokio::test]
    async fn dry_run_reports_state_without_writing() {
        let env = Env::new(ComplianceMode::Enforce).await;
        let p = plan(&env.pool, &CutoverOptions::default()).await.unwrap();
        assert_eq!(p.wallets.len(), 3);
        assert_eq!(p.total_free, 1_025_005 * UNIT / 10);
        assert_eq!(p.total_collateral, 1_100 * UNIT);
        assert_eq!(p.total_open_positions, 2);
        assert!(p.blocking_issues().is_empty());
        // Deterministic.
        assert_eq!(
            p.sha256,
            plan(&env.pool, &CutoverOptions::default())
                .await
                .unwrap()
                .sha256
        );
        let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cutover_runs")
            .fetch_one(&env.pool)
            .await
            .unwrap();
        assert_eq!(runs, 0);
        // require_flat blocks wallets with open positions.
        let flat = plan(&env.pool, &CutoverOptions { require_flat: true })
            .await
            .unwrap();
        assert_eq!(flat.blocking_issues().len(), 2);
    }

    #[tokio::test]
    async fn inconsistent_books_block_the_snapshot() {
        let env = Env::new(ComplianceMode::Enforce).await;
        sqlx::query("UPDATE accounts SET collateral_locked = 900.0 WHERE wallet_address = 'GBOB'")
            .execute(&env.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE accounts SET balance = -1.0 WHERE wallet_address = 'GCAROL'")
            .execute(&env.pool)
            .await
            .unwrap();
        match env
            .cutover()
            .snapshot("t", &CutoverOptions::default(), "ops", 1)
            .await
        {
            Err(CutoverError::Blocked(issues)) => {
                assert_eq!(issues.len(), 2, "{issues:?}");
                assert!(issues.iter().any(|i| i.contains("does not match")));
                assert!(issues.iter().any(|i| i.contains("negative free")));
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn full_cutover_reconciles_exactly_and_keeps_offchain_records() {
        let env = Env::new(ComplianceMode::Enforce).await;
        let c = env.cutover();
        let run = c
            .snapshot("mainnet cutover", &CutoverOptions::default(), "ops", 10)
            .await
            .unwrap();
        assert_eq!(
            settlement_mode(&env.pool, ALICE).await.unwrap(),
            "migrating"
        );
        // Only one run in flight.
        assert!(matches!(
            c.snapshot("again", &CutoverOptions::default(), "ops", 11)
                .await,
            Err(CutoverError::RunInFlight)
        ));

        let m = c.migrate(&run.id, "ops", 20).await.unwrap();
        assert_eq!((m.migrated, m.held, m.failed, m.complete), (3, 0, 0, true));
        assert_eq!(
            env.escrow.balance(BOB).await.unwrap(),
            EscrowBalance {
                free: 25_005 * UNIT / 10,
                collateral: 1_100 * UNIT
            }
        );
        assert_eq!(settlement_mode(&env.pool, BOB).await.unwrap(), "onchain");

        let r = c.verify(&run.id, "ops", 30).await.unwrap();
        assert!(r.is_clean(), "{r:?}");
        assert_eq!(r.snapshot_total, r.onchain_total);
        assert_eq!(c.run(&run.id).await.unwrap().status, "verified");

        // Users were not deleted: the off-chain rows are untouched.
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts")
            .fetch_one(&env.pool)
            .await
            .unwrap();
        assert_eq!(n, 3);
        // And the snapshot cannot be edited after the fact.
        assert!(
            sqlx::query("UPDATE cutover_snapshot_wallets SET free_stroops = 0")
                .execute(&env.pool)
                .await
                .is_err()
        );
        // A later plan skips wallets already on-chain.
        assert_eq!(
            plan(&env.pool, &CutoverOptions::default())
                .await
                .unwrap()
                .already_onchain,
            3
        );
    }

    #[tokio::test]
    async fn failures_resume_without_double_credit() {
        let env = Env::new(ComplianceMode::Enforce).await;
        let c = env.cutover();
        let run = c
            .snapshot("t", &CutoverOptions::default(), "ops", 1)
            .await
            .unwrap();
        env.escrow.fail_for(BOB, true);
        let m = c.migrate(&run.id, "ops", 2).await.unwrap();
        assert_eq!((m.migrated, m.failed, m.complete), (2, 1, false));
        assert_eq!(c.run(&run.id).await.unwrap().status, "migrating");
        assert!(!c.verify(&run.id, "ops", 3).await.unwrap().is_clean());

        env.escrow.fail_for(BOB, false);
        let m = c.migrate(&run.id, "ops", 4).await.unwrap();
        assert!(m.complete);
        // Re-running a completed migration changes nothing.
        let before = env.escrow.total();
        assert!(
            c.migrate(&run.id, "ops", 5).await.is_err(),
            "run is now 'migrated'"
        );
        assert_eq!(env.escrow.total(), before);
        assert!(c.verify(&run.id, "ops", 6).await.unwrap().is_clean());
    }

    #[tokio::test]
    async fn drift_after_snapshot_is_caught() {
        let env = Env::new(ComplianceMode::Enforce).await;
        let c = env.cutover();
        let run = c
            .snapshot("t", &CutoverOptions::default(), "ops", 1)
            .await
            .unwrap();
        // A handler that ignored the 'migrating' freeze.
        sqlx::query("UPDATE accounts SET balance = balance + 5 WHERE wallet_address = 'GALICE'")
            .execute(&env.pool)
            .await
            .unwrap();
        let m = c.migrate(&run.id, "ops", 2).await.unwrap();
        assert_eq!(m.failed, 1);
        assert_eq!(
            env.escrow.balance(ALICE).await.unwrap(),
            EscrowBalance::default()
        );
    }

    #[tokio::test]
    async fn sanctioned_wallets_are_held_offchain_and_books_still_conserve() {
        let env = Env::new(ComplianceMode::Enforce).await;
        sqlx::query("INSERT INTO accounts (wallet_address, balance) VALUES ('GSANCTIONED', 777.0)")
            .execute(&env.pool)
            .await
            .unwrap();
        let c = env.cutover();
        let run = c
            .snapshot("t", &CutoverOptions::default(), "ops", 1)
            .await
            .unwrap();
        let m = c.migrate(&run.id, "ops", 2).await.unwrap();
        assert_eq!((m.migrated, m.held, m.complete), (3, 1, true));
        assert_eq!(
            env.escrow.balance(SANCTIONED).await.unwrap(),
            EscrowBalance::default()
        );
        assert_eq!(
            settlement_mode(&env.pool, SANCTIONED).await.unwrap(),
            "offchain"
        );
        let r = c.verify(&run.id, "ops", 3).await.unwrap();
        assert!(r.is_clean(), "{r:?}");
        assert_eq!(r.held_total.free, 777 * UNIT);
    }

    #[tokio::test]
    async fn unpinned_or_changed_escrow_is_refused() {
        let mut env = Env::new(ComplianceMode::Enforce).await;
        let run = env
            .cutover()
            .snapshot("t", &CutoverOptions::default(), "ops", 1)
            .await
            .unwrap();
        env.hashes
            .0
            .insert(ESCROW_ID.into(), sha256_hex(b"upgraded"));
        assert!(matches!(
            env.cutover().migrate(&run.id, "ops", 2).await,
            Err(CutoverError::Pins(_))
        ));
        assert_eq!(env.escrow.total(), EscrowBalance::default());
    }

    #[tokio::test]
    async fn verify_detects_out_of_band_escrow_changes() {
        let env = Env::new(ComplianceMode::Enforce).await;
        let c = env.cutover();
        let run = c
            .snapshot("t", &CutoverOptions::default(), "ops", 1)
            .await
            .unwrap();
        c.migrate(&run.id, "ops", 2).await.unwrap();
        env.escrow.force_balance(
            ALICE,
            EscrowBalance {
                free: 1,
                collateral: 0,
            },
        );
        let r = c.verify(&run.id, "ops", 3).await.unwrap();
        assert!(!r.is_clean());
        assert!(!r.conserved);
        assert_eq!(r.mismatches[0].wallet, ALICE);
        assert_eq!(c.run(&run.id).await.unwrap().status, "migrated");
    }

    #[tokio::test]
    async fn rollback_debits_exactly_what_was_credited() {
        let env = Env::new(ComplianceMode::Enforce).await;
        let c = env.cutover();
        let run = c
            .snapshot("t", &CutoverOptions::default(), "ops", 1)
            .await
            .unwrap();
        env.escrow.fail_for(CAROL, true); // irrelevant: Carol is empty
        env.escrow.fail_for(BOB, true);
        c.migrate(&run.id, "ops", 2).await.unwrap();
        env.escrow.fail_for(BOB, false);

        assert!(
            c.rollback(&run.id, "ops", " ", 3).await.is_err(),
            "reason required"
        );
        let r = c
            .rollback(&run.id, "ops", "rehearsal abort", 3)
            .await
            .unwrap();
        assert!(r.complete);
        assert_eq!(env.escrow.total(), EscrowBalance::default());
        for w in [ALICE, BOB, CAROL] {
            assert_eq!(settlement_mode(&env.pool, w).await.unwrap(), "offchain");
        }
        assert_eq!(c.run(&run.id).await.unwrap().status, "rolled_back");
        // Idempotent, and a new run can start afterwards.
        assert!(
            c.rollback(&run.id, "ops", "again", 4)
                .await
                .unwrap()
                .complete
        );
        assert!(c
            .snapshot("retry", &CutoverOptions::default(), "ops", 5)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn rehearsal_on_a_database_copy_passes() {
        let env = Env::new(ComplianceMode::Enforce).await;
        let dir = std::env::temp_dir().join(format!("zenith-rehearsal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // In-memory sources cannot be copied; that is reported, not ignored.
        assert!(copy_database(&env.pool, &dir.join("from-memory.db"))
            .await
            .is_err());

        // Stand-in for the live database file.
        let live_path = dir.join("live.db");
        copy_database_for_test(&env.pool, &live_path).await;
        let live = SqlitePool::connect(&format!("sqlite://{}", live_path.display()))
            .await
            .unwrap();

        let copy_path = dir.join("rehearsal.db");
        copy_database(&live, &copy_path).await.unwrap();
        assert!(
            copy_database(&live, &copy_path).await.is_err(),
            "never overwrites"
        );

        let copy = SqlitePool::connect(&format!("sqlite://{}", copy_path.display()))
            .await
            .unwrap();
        let gate = ComplianceGate::new(
            copy.clone(),
            Arc::new(DenylistProvider::new("t", Vec::<(String, String)>::new())),
            CompliancePolicy {
                mode: ComplianceMode::Enforce,
                ..CompliancePolicy::default()
            },
        );
        let escrow = InMemoryEscrow::new(ESCROW_ID);
        let c = Cutover {
            pool: &copy,
            escrow: &escrow,
            compliance: &gate,
            pins: &env.pins,
            hashes: &env.hashes,
        };
        let report = rehearse(&c, &CutoverOptions::default(), "ops", 100)
            .await
            .unwrap();
        assert!(report.passed(), "{report:?}");
        assert_eq!(report.run.status, "rolled_back");
        // The live database was not touched by the rehearsal.
        let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cutover_runs")
            .fetch_one(&live)
            .await
            .unwrap();
        assert_eq!(runs, 0);
        copy.close().await;
        live.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
