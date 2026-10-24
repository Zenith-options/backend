//! Post-restore recovery planning for disaster recovery.
//!
//! Restoring the SQLite file (see `docs/runbooks/disaster-recovery.md`) only
//! brings back the state as of the backup point. Two kinds of durable state
//! need more than a file copy before the backend can safely resume:
//!
//! * **Indexer cursors and checkpoints.** The restored cursors sit at (or
//!   before) the backup point, and the checkpoint table may contain holes.
//!   [`plan_indexer_recovery`] works out where each stream must resume from
//!   and which ledger ranges must be backfilled so indexing continues with
//!   no gaps — and whether those ranges are still inside the RPC retention
//!   window or must come from the LedgerMeta archive instead.
//! * **Account sequence numbers.** A Stellar source account's sequence
//!   number lives on-chain. Any value in the backup is stale the moment a
//!   transaction lands after the backup point, so
//!   [`resolve_sequence`] always takes the on-chain value and reports how
//!   far the backup drifted.
//!
//! The planning logic is pure so it can be unit tested; [`load_indexer_state`]
//! reads the inputs from a restored database and tolerates the indexer
//! tables not existing yet.

use serde::Serialize;
use sqlx::SqlitePool;

/// Default Soroban RPC event retention: 17_280 ledgers (~24h at ~5s per
/// ledger). Ranges older than this can't be re-read from RPC and must be
/// backfilled from the LedgerMeta archive.
pub const DEFAULT_RPC_RETENTION_LEDGERS: u64 = 17_280;

/// A completed indexer checkpoint, half-open `[from_ledger, to_ledger)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Checkpoint {
    pub from_ledger: u64,
    pub to_ledger: u64,
}

/// A durable per-stream cursor: the last ledger fully ingested.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StreamCursor {
    pub stream: String,
    pub ledger: u64,
}

/// Where a backfill range has to be read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackfillSource {
    /// Still within the RPC retention window.
    Rpc,
    /// Older than the RPC window: needs the LedgerMeta (Galexie) archive.
    LedgerMetaArchive,
}

/// A half-open ledger range `[from_ledger, to_ledger)` to re-ingest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BackfillRange {
    pub from_ledger: u64,
    pub to_ledger: u64,
    pub source: BackfillSource,
}

/// Per-stream resume point after a restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StreamResume {
    pub stream: String,
    /// First ledger the stream must ingest next.
    pub resume_from: u64,
}

/// What an operator must do to bring indexing back to the chain tip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexerRecoveryPlan {
    pub chain_tip: u64,
    pub streams: Vec<StreamResume>,
    /// Holes in the checkpoint history, plus the catch-up range from the
    /// restored cursors to the chain tip. Sorted, non-overlapping.
    pub backfill: Vec<BackfillRange>,
    /// Problems that need a human before the indexer is restarted.
    pub warnings: Vec<String>,
}

impl IndexerRecoveryPlan {
    /// True when every range can be re-read from RPC, i.e. no archive
    /// backfill is needed.
    pub fn rpc_only(&self) -> bool {
        self.backfill
            .iter()
            .all(|r| r.source == BackfillSource::Rpc)
    }
}

/// Builds the recovery plan from the restored checkpoints and cursors.
///
/// * Gaps between checkpoints are backfilled.
/// * Each stream resumes at `cursor + 1`; the catch-up range starts at the
///   lowest resume point so no stream skips a ledger.
/// * A range is split at the RPC retention boundary so the part older than
///   `chain_tip - rpc_retention` is marked for the archive.
pub fn plan_indexer_recovery(
    checkpoints: &[Checkpoint],
    cursors: &[StreamCursor],
    chain_tip: u64,
    rpc_retention: u64,
) -> IndexerRecoveryPlan {
    let mut warnings = Vec::new();
    let mut raw: Vec<(u64, u64)> = Vec::new();

    let mut sorted: Vec<Checkpoint> = checkpoints
        .iter()
        .copied()
        .filter(|c| c.to_ledger > c.from_ledger)
        .collect();
    sorted.sort_by_key(|c| (c.from_ledger, c.to_ledger));

    // Gaps inside the recorded history.
    let mut covered_to: Option<u64> = None;
    for c in &sorted {
        if let Some(end) = covered_to {
            if c.from_ledger > end {
                raw.push((end, c.from_ledger));
            }
            covered_to = Some(end.max(c.to_ledger));
        } else {
            covered_to = Some(c.to_ledger);
        }
    }

    let streams: Vec<StreamResume> = cursors
        .iter()
        .map(|c| StreamResume {
            stream: c.stream.clone(),
            resume_from: c.ledger + 1,
        })
        .collect();

    for c in cursors {
        if c.ledger > chain_tip {
            warnings.push(format!(
                "stream '{}' cursor {} is ahead of chain tip {}: wrong network or stale tip",
                c.stream, c.ledger, chain_tip
            ));
        }
        if let Some(end) = covered_to {
            // Cursors are inclusive, checkpoints half-open.
            if c.ledger + 1 > end {
                warnings.push(format!(
                    "stream '{}' cursor {} is past the last checkpoint end {}: \
                     the checkpoint for the final batch is missing",
                    c.stream, c.ledger, end
                ));
            }
        }
    }

    // Catch-up from the lowest resume point (or the end of checkpoints when
    // no cursor exists) to the chain tip.
    let catch_up_from = streams.iter().map(|s| s.resume_from).min().or(covered_to);
    match catch_up_from {
        Some(from) if from <= chain_tip => raw.push((from, chain_tip + 1)),
        Some(_) => {}
        None => warnings.push(
            "no cursors or checkpoints found: the indexer will start from its \
             configured genesis ledger"
                .to_string(),
        ),
    }

    let boundary = chain_tip.saturating_sub(rpc_retention);
    let mut backfill = Vec::new();
    for (from, to) in merge_ranges(raw) {
        if to <= boundary {
            backfill.push(range(from, to, BackfillSource::LedgerMetaArchive));
        } else if from >= boundary {
            backfill.push(range(from, to, BackfillSource::Rpc));
        } else {
            backfill.push(range(from, boundary, BackfillSource::LedgerMetaArchive));
            backfill.push(range(boundary, to, BackfillSource::Rpc));
        }
    }

    IndexerRecoveryPlan {
        chain_tip,
        streams,
        backfill,
        warnings,
    }
}

fn range(from_ledger: u64, to_ledger: u64, source: BackfillSource) -> BackfillRange {
    BackfillRange {
        from_ledger,
        to_ledger,
        source,
    }
}

fn merge_ranges(mut ranges: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    ranges.retain(|(a, b)| b > a);
    ranges.sort();
    let mut out: Vec<(u64, u64)> = Vec::new();
    for (a, b) in ranges {
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// Outcome of reconciling a source account's sequence number after restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SequenceResolution {
    /// The value to use for the next transaction's base sequence. Always
    /// the on-chain value.
    pub use_sequence: i64,
    /// `on_chain - backup`. Positive means transactions landed after the
    /// backup point; their outcomes must be recovered by the indexer
    /// backfill, not from the database.
    pub drift: i64,
}

/// Errors that block resuming submissions for an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SequenceError {
    /// The backup claims a higher sequence than the chain. That can only
    /// happen with a backup from a different network or a reset testnet;
    /// submitting would fail with `tx_bad_seq` forever.
    BackupAheadOfChain { backup: i64, on_chain: i64 },
}

impl std::fmt::Display for SequenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BackupAheadOfChain { backup, on_chain } => write!(
                f,
                "backup sequence {backup} is ahead of on-chain sequence {on_chain}: \
                 the backup is from a different network or the network was reset"
            ),
        }
    }
}

impl std::error::Error for SequenceError {}

/// Reconciles a restored sequence number against the chain. The chain is
/// the source of truth; the backup value is only used to detect drift.
pub fn resolve_sequence(
    backup: Option<i64>,
    on_chain: i64,
) -> Result<SequenceResolution, SequenceError> {
    match backup {
        Some(b) if b > on_chain => Err(SequenceError::BackupAheadOfChain {
            backup: b,
            on_chain,
        }),
        Some(b) => Ok(SequenceResolution {
            use_sequence: on_chain,
            drift: on_chain - b,
        }),
        None => Ok(SequenceResolution {
            use_sequence: on_chain,
            drift: 0,
        }),
    }
}

async fn table_exists(pool: &SqlitePool, name: &str) -> Result<bool, sqlx::Error> {
    let n: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1")
            .bind(name)
            .fetch_one(pool)
            .await?;
    Ok(n > 0)
}

/// Reads completed checkpoints and stream cursors from a restored database.
/// Missing tables yield empty lists rather than an error, so the planner
/// also works against databases created before the indexer existed.
pub async fn load_indexer_state(
    pool: &SqlitePool,
) -> Result<(Vec<Checkpoint>, Vec<StreamCursor>), sqlx::Error> {
    let mut checkpoints = Vec::new();
    if table_exists(pool, "indexer_checkpoints").await? {
        let rows: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT from_ledger, to_ledger FROM indexer_checkpoints \
             WHERE status = 'complete' ORDER BY from_ledger",
        )
        .fetch_all(pool)
        .await?;
        checkpoints = rows
            .into_iter()
            .map(|(f, t)| Checkpoint {
                from_ledger: f.max(0) as u64,
                to_ledger: t.max(0) as u64,
            })
            .collect();
    }

    let mut cursors = Vec::new();
    if table_exists(pool, "indexer_cursors").await? {
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT stream, ledger FROM indexer_cursors ORDER BY stream")
                .fetch_all(pool)
                .await?;
        cursors = rows
            .into_iter()
            .map(|(stream, ledger)| StreamCursor {
                stream,
                ledger: ledger.max(0) as u64,
            })
            .collect();
    }

    Ok((checkpoints, cursors))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cp(from_ledger: u64, to_ledger: u64) -> Checkpoint {
        Checkpoint {
            from_ledger,
            to_ledger,
        }
    }

    fn cur(stream: &str, ledger: u64) -> StreamCursor {
        StreamCursor {
            stream: stream.into(),
            ledger,
        }
    }

    #[test]
    fn contiguous_history_only_needs_catch_up() {
        let plan = plan_indexer_recovery(
            &[cp(100, 200), cp(200, 300)],
            &[cur("events", 299)],
            350,
            DEFAULT_RPC_RETENTION_LEDGERS,
        );
        assert_eq!(plan.streams[0].resume_from, 300);
        assert_eq!(plan.backfill, vec![range(300, 351, BackfillSource::Rpc)]);
        assert!(plan.warnings.is_empty());
        assert!(plan.rpc_only());
    }

    #[test]
    fn holes_between_checkpoints_are_backfilled() {
        let plan = plan_indexer_recovery(
            &[cp(100, 200), cp(250, 300), cp(310, 400)],
            &[cur("events", 399)],
            400,
            DEFAULT_RPC_RETENTION_LEDGERS,
        );
        assert_eq!(
            plan.backfill,
            vec![
                range(200, 250, BackfillSource::Rpc),
                range(300, 310, BackfillSource::Rpc),
                range(400, 401, BackfillSource::Rpc),
            ]
        );
    }

    #[test]
    fn overlapping_checkpoints_are_not_gaps() {
        let plan = plan_indexer_recovery(
            &[cp(100, 250), cp(200, 300)],
            &[cur("events", 299)],
            299,
            DEFAULT_RPC_RETENTION_LEDGERS,
        );
        assert!(plan.backfill.is_empty());
    }

    #[test]
    fn slowest_stream_sets_the_catch_up_start() {
        let plan = plan_indexer_recovery(
            &[cp(0, 500)],
            &[cur("fast", 499), cur("slow", 449)],
            600,
            DEFAULT_RPC_RETENTION_LEDGERS,
        );
        assert_eq!(plan.backfill, vec![range(450, 601, BackfillSource::Rpc)]);
        let slow = plan.streams.iter().find(|s| s.stream == "slow").unwrap();
        assert_eq!(slow.resume_from, 450);
    }

    #[test]
    fn ranges_older_than_rpc_retention_go_to_the_archive() {
        let plan = plan_indexer_recovery(&[cp(0, 1_000)], &[cur("events", 999)], 5_000, 1_000);
        // boundary = 4000
        assert_eq!(
            plan.backfill,
            vec![
                range(1_000, 4_000, BackfillSource::LedgerMetaArchive),
                range(4_000, 5_001, BackfillSource::Rpc),
            ]
        );
        assert!(!plan.rpc_only());
    }

    #[test]
    fn cursor_ahead_of_chain_is_flagged() {
        let plan = plan_indexer_recovery(&[cp(0, 1_000)], &[cur("events", 999)], 500, 1_000);
        assert!(plan
            .warnings
            .iter()
            .any(|w| w.contains("ahead of chain tip")));
        assert!(plan.backfill.is_empty());
    }

    #[test]
    fn cursor_past_last_checkpoint_is_flagged() {
        let plan = plan_indexer_recovery(&[cp(0, 100)], &[cur("events", 150)], 200, 1_000);
        assert!(plan
            .warnings
            .iter()
            .any(|w| w.contains("past the last checkpoint")));
    }

    #[test]
    fn empty_state_warns_and_plans_nothing() {
        let plan = plan_indexer_recovery(&[], &[], 1_000, 1_000);
        assert!(plan.backfill.is_empty());
        assert_eq!(plan.warnings.len(), 1);
    }

    #[test]
    fn sequence_always_uses_chain_value() {
        assert_eq!(
            resolve_sequence(Some(10), 15),
            Ok(SequenceResolution {
                use_sequence: 15,
                drift: 5
            })
        );
        assert_eq!(
            resolve_sequence(None, 7),
            Ok(SequenceResolution {
                use_sequence: 7,
                drift: 0
            })
        );
    }

    #[test]
    fn sequence_backup_ahead_of_chain_is_refused() {
        assert_eq!(
            resolve_sequence(Some(20), 15),
            Err(SequenceError::BackupAheadOfChain {
                backup: 20,
                on_chain: 15
            })
        );
    }

    #[tokio::test]
    async fn load_tolerates_missing_tables_and_reads_present_ones() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        let (c, s) = load_indexer_state(&pool).await.unwrap();
        assert!(c.is_empty() && s.is_empty());

        sqlx::query(
            "CREATE TABLE indexer_checkpoints (from_ledger INTEGER, to_ledger INTEGER, status TEXT);
             CREATE TABLE indexer_cursors (stream TEXT PRIMARY KEY, ledger INTEGER, paging_token TEXT);
             INSERT INTO indexer_checkpoints VALUES (10, 20, 'complete'), (20, 30, 'failed');
             INSERT INTO indexer_cursors VALUES ('events', 19, 't');",
        )
        .execute(&pool)
        .await
        .unwrap();

        let (c, s) = load_indexer_state(&pool).await.unwrap();
        assert_eq!(c, vec![cp(10, 20)]);
        assert_eq!(s, vec![cur("events", 19)]);
    }
}
