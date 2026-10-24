//! Data retention and cold archival.
//!
//! Once the event indexer and backfill run against real ledgers the SQLite
//! database grows without bound. Regulated settlement also needs historical
//! records kept long-term — but not hot. Each [`RetentionPolicy`] moves rows
//! older than its window out of the database:
//!
//! 1. `BEGIN IMMEDIATE` (takes the write lock up front);
//! 2. select one batch of expired rows, oldest rowid first;
//! 3. refuse the batch if another table still references any of its rows
//!    (deleting would cascade or orphan them — archive the child first);
//! 4. write the rows as JSON lines to `<archive_root>/<table>/<file>.jsonl`
//!    via a temp file, fsync, rename;
//! 5. record an `archive_manifests` row with the real sha256, row count and
//!    age range, delete exactly those rows, commit.
//!
//! A crash before commit leaves the rows in place (and at worst an orphan
//! archive file with no manifest), so data is never deleted without a
//! durable, checksummed copy. [`verify_archive`] re-checks a file against
//! its manifest and [`restore_archive`] loads a batch back for an audit.
//!
//! Table and column names can't be bound as parameters, so they are
//! validated (identifier charset, existence in the schema) and quoted.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionPolicy {
    pub table_name: String,
    /// Column holding the row's age: ISO-8601 text
    /// (`YYYY-MM-DDTHH:MM:SS...`) or integer Unix seconds.
    pub age_column: String,
    pub retention_days: u32,
    /// Archive before deleting. Required for [`ARCHIVE_REQUIRED_TABLES`].
    pub archive: bool,
    pub batch_size: u32,
}

/// Never purged: the settlement record of truth, and the bookkeeping the
/// retention and recovery machinery itself depends on.
pub const PROTECTED_TABLES_DENYLIST: &[&str] = &[
    "sponsored_transactions",
    "contract_wasm_history",
    "network_metadata",
    "accounts",
    "positions",
    "ledger_accounts",
    "ledger_entries",
    "settlement_fixings",
    "liquidation_deficits",
    "insurance_reserve_state",
    "insurance_reserve_entries",
    "indexer_cursors",
    "indexer_checkpoints",
    "archive_manifests",
    "_sqlx_migrations",
];

/// Settlement evidence: may leave the hot database, but only into an
/// archive.
pub const ARCHIVE_REQUIRED_TABLES: &[&str] = &[
    "chain_events",
    "series_created",
    "option_minted",
    "option_exercised",
    "collateral_deposited",
    "settled",
    "liquidations",
    "shadow_comparisons",
];

/// Upper bound on a batch, so one batch never holds the write lock long.
pub const MAX_BATCH_SIZE: u32 = 10_000;

/// Built-in policies for the tables that grow with chain activity. Tables
/// missing from the schema are skipped by [`run_policies`].
pub fn default_policies() -> Vec<RetentionPolicy> {
    let p = |table: &str, col: &str, days: u32, archive: bool| RetentionPolicy {
        table_name: table.into(),
        age_column: col.into(),
        retention_days: days,
        archive,
        batch_size: 5_000,
    };
    vec![
        // Projections reference chain_events, so they go first.
        p("series_created", "created_at", 180, true),
        p("option_minted", "created_at", 180, true),
        p("option_exercised", "created_at", 180, true),
        p("collateral_deposited", "created_at", 180, true),
        p("settled", "created_at", 180, true),
        p("chain_events", "ingested_at", 180, true),
        p("shadow_comparisons", "observed_at", 90, true),
        p("price_candles", "minute", 365, true),
        p("keeper_job_executions", "created_at", 30, false),
    ]
}

#[derive(Debug)]
pub enum RetentionError {
    Db(sqlx::Error),
    Io(std::io::Error),
    Protected(String),
    ArchiveRequired(String),
    InvalidPolicy(String),
    /// Rows in `child` still reference rows in the batch.
    Referenced {
        table: String,
        child: String,
        rows: i64,
    },
    ManifestNotFound(String),
    ChecksumMismatch {
        manifest_id: String,
        expected: String,
        actual: String,
    },
}

impl std::fmt::Display for RetentionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(e) => write!(f, "database error: {e}"),
            Self::Io(e) => write!(f, "archive I/O error: {e}"),
            Self::Protected(t) => write!(
                f,
                "Table '{t}' is on the protected denylist and cannot be purged."
            ),
            Self::ArchiveRequired(t) => write!(
                f,
                "Table '{t}' holds settlement evidence and may only be purged with archive = true."
            ),
            Self::InvalidPolicy(m) => write!(f, "invalid retention policy: {m}"),
            Self::Referenced { table, child, rows } => write!(
                f,
                "{rows} row(s) in '{child}' still reference expired '{table}' rows; \
                 archive '{child}' first"
            ),
            Self::ManifestNotFound(id) => write!(f, "archive manifest {id} not found"),
            Self::ChecksumMismatch {
                manifest_id,
                expected,
                actual,
            } => write!(
                f,
                "archive {manifest_id} checksum mismatch: manifest {expected}, file {actual}"
            ),
        }
    }
}

impl std::error::Error for RetentionError {}

impl From<sqlx::Error> for RetentionError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

impl From<std::io::Error> for RetentionError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// What one policy run did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RetentionOutcome {
    pub table: String,
    pub rows_deleted: u64,
    pub rows_archived: u64,
    pub batches: u32,
    pub manifest_ids: Vec<String>,
}

fn is_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.starts_with(|c: char| c.is_ascii_digit())
}

fn quote(ident: &str) -> String {
    format!("\"{ident}\"")
}

/// Static checks that don't need the database.
pub fn validate_policy(policy: &RetentionPolicy) -> Result<(), RetentionError> {
    let t = &policy.table_name;
    if RetentionService::is_table_protected(t) {
        return Err(RetentionError::Protected(t.clone()));
    }
    if ARCHIVE_REQUIRED_TABLES.contains(&t.as_str()) && !policy.archive {
        return Err(RetentionError::ArchiveRequired(t.clone()));
    }
    if !is_identifier(t) || !is_identifier(&policy.age_column) {
        return Err(RetentionError::InvalidPolicy(format!(
            "'{t}.{}' is not a plain identifier",
            policy.age_column
        )));
    }
    if policy.retention_days == 0 {
        return Err(RetentionError::InvalidPolicy(
            "retention_days must be at least 1".into(),
        ));
    }
    if policy.batch_size == 0 || policy.batch_size > MAX_BATCH_SIZE {
        return Err(RetentionError::InvalidPolicy(format!(
            "batch_size must be 1..={MAX_BATCH_SIZE}"
        )));
    }
    Ok(())
}

struct TableShape {
    columns: Vec<String>,
    age_is_integer: bool,
    /// (child table, child column, parent column)
    referenced_by: Vec<(String, String, String)>,
}

async fn table_exists(pool: &SqlitePool, table: &str) -> Result<bool, sqlx::Error> {
    let n: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1")
            .bind(table)
            .fetch_one(pool)
            .await?;
    Ok(n > 0)
}

async fn table_shape(
    pool: &SqlitePool,
    policy: &RetentionPolicy,
) -> Result<TableShape, RetentionError> {
    let cols: Vec<(String, String)> =
        sqlx::query_as("SELECT name, type FROM pragma_table_info(?1) ORDER BY cid")
            .bind(&policy.table_name)
            .fetch_all(pool)
            .await?;
    if cols.is_empty() {
        return Err(RetentionError::InvalidPolicy(format!(
            "table '{}' does not exist",
            policy.table_name
        )));
    }
    let age_type = cols
        .iter()
        .find(|(n, _)| n == &policy.age_column)
        .map(|(_, t)| t.to_ascii_uppercase())
        .ok_or_else(|| {
            RetentionError::InvalidPolicy(format!(
                "column '{}.{}' does not exist",
                policy.table_name, policy.age_column
            ))
        })?;

    let referenced_by: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT m.name, fk.\"from\", COALESCE(fk.\"to\", 'rowid')
           FROM sqlite_master m, pragma_foreign_key_list(m.name) fk
          WHERE m.type = 'table' AND fk.\"table\" = ?1 AND m.name <> ?1",
    )
    .bind(&policy.table_name)
    .fetch_all(pool)
    .await?;

    Ok(TableShape {
        columns: cols.into_iter().map(|(n, _)| n).collect(),
        age_is_integer: age_type.contains("INT"),
        referenced_by,
    })
}

/// SQL expression rendering one row as a JSON object. BLOBs become
/// `{"$hex": "..."}` so the archive stays plain text.
fn row_json_expr(columns: &[String]) -> String {
    let parts: Vec<String> = columns
        .iter()
        .map(|c| {
            let key = serde_json::to_string(c).expect("string serialises");
            let q = quote(c);
            format!(
                "'{key}:' || CASE WHEN typeof({q}) = 'blob' \
                 THEN json_object('$hex', hex({q})) ELSE json_quote({q}) END",
                key = key.replace('\'', "''")
            )
        })
        .collect();
    format!("'{{' || {} || '}}'", parts.join(" || ',' || "))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Writes `data` to `path` durably: temp file, fsync, rename, fsync dir.
fn write_durably(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().expect("archive path has a parent");
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("jsonl.partial");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// Archives (if configured) and deletes one batch. Returns rows removed and
/// the manifest id, or `None` when nothing has expired.
async fn run_batch(
    pool: &SqlitePool,
    policy: &RetentionPolicy,
    shape: &TableShape,
    archive_root: &Path,
) -> Result<Option<(u64, Option<String>)>, RetentionError> {
    let table = quote(&policy.table_name);
    let age = quote(&policy.age_column);
    let cutoff_expr = if shape.age_is_integer {
        "CAST(strftime('%s', 'now', ?1) AS INTEGER)"
    } else {
        "strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?1)"
    };
    let window = format!("-{} days", policy.retention_days);

    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;

    let select = format!(
        "SELECT rowid, {json}, CAST({age} AS TEXT) FROM {table} \
         WHERE {age} < {cutoff_expr} ORDER BY rowid LIMIT ?2",
        json = row_json_expr(&shape.columns),
    );
    let rows = sqlx::query(&select)
        .bind(&window)
        .bind(policy.batch_size as i64)
        .fetch_all(&mut *tx)
        .await?;
    if rows.is_empty() {
        return Ok(None);
    }

    let mut rowids = Vec::with_capacity(rows.len());
    let mut body = String::new();
    let mut ages = Vec::with_capacity(rows.len());
    for r in &rows {
        rowids.push(r.try_get::<i64, _>(0)?);
        body.push_str(&r.try_get::<String, _>(1)?);
        body.push('\n');
        if let Some(ts) = r.try_get::<Option<String>, _>(2)? {
            ages.push(ts);
        }
    }
    let oldest = ages.iter().min().cloned();
    let newest = ages.iter().max().cloned();
    let rowid_json = serde_json::to_string(&rowids).expect("ids serialise");

    for (child, child_col, parent_col) in &shape.referenced_by {
        let q = format!(
            "SELECT COUNT(*) FROM {child} WHERE {cc} IN \
             (SELECT {pc} FROM {table} WHERE rowid IN (SELECT value FROM json_each(?1)))",
            child = quote(child),
            cc = quote(child_col),
            pc = quote(parent_col),
        );
        let n: i64 = sqlx::query_scalar(&q)
            .bind(&rowid_json)
            .fetch_one(&mut *tx)
            .await?;
        if n > 0 {
            return Err(RetentionError::Referenced {
                table: policy.table_name.clone(),
                child: child.clone(),
                rows: n,
            });
        }
    }

    let mut manifest_id = None;
    if policy.archive {
        let id = uuid::Uuid::new_v4().to_string();
        let stamp = oldest
            .as_deref()
            .unwrap_or("unknown")
            .get(..10)
            .unwrap_or("unknown");
        let rel = format!(
            "{t}/{t}_{stamp}_{id}.jsonl",
            t = policy.table_name,
            stamp = stamp.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "_"),
        );
        let path = archive_root.join(&rel);
        write_durably(&path, body.as_bytes())?;

        sqlx::query(
            "INSERT INTO archive_manifests
               (id, table_name, file_path, row_count, checksum, from_timestamp, to_timestamp, format)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'jsonl-v1')",
        )
        .bind(&id)
        .bind(&policy.table_name)
        .bind(&rel)
        .bind(rows.len() as i64)
        .bind(format!("sha256:{}", sha256_hex(body.as_bytes())))
        .bind(oldest.unwrap_or_default())
        .bind(newest.unwrap_or_default())
        .execute(&mut *tx)
        .await?;
        manifest_id = Some(id);
    }

    let deleted = sqlx::query(&format!(
        "DELETE FROM {table} WHERE rowid IN (SELECT value FROM json_each(?1))"
    ))
    .bind(&rowid_json)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(Some((deleted, manifest_id)))
}

pub struct RetentionService;

impl RetentionService {
    pub fn is_table_protected(table: &str) -> bool {
        PROTECTED_TABLES_DENYLIST.contains(&table)
    }

    /// Runs one policy to completion (until no expired rows remain),
    /// archiving under `archive_root` when the policy asks for it.
    pub async fn execute_policy(
        pool: &SqlitePool,
        policy: &RetentionPolicy,
        archive_root: &Path,
    ) -> Result<RetentionOutcome, RetentionError> {
        validate_policy(policy)?;
        let shape = table_shape(pool, policy).await?;
        let mut outcome = RetentionOutcome {
            table: policy.table_name.clone(),
            ..Default::default()
        };
        while let Some((deleted, manifest)) = run_batch(pool, policy, &shape, archive_root).await? {
            outcome.batches += 1;
            outcome.rows_deleted += deleted;
            if let Some(id) = manifest {
                outcome.rows_archived += deleted;
                outcome.manifest_ids.push(id);
            }
        }
        if outcome.rows_deleted > 0 {
            tracing::info!(
                table = %outcome.table,
                rows_deleted = outcome.rows_deleted,
                rows_archived = outcome.rows_archived,
                batches = outcome.batches,
                "retention policy applied"
            );
        }
        Ok(outcome)
    }

    /// Compatibility wrapper: archives under `$ARCHIVE_DIR` (default
    /// `archives/`) and returns the number of rows removed.
    pub async fn execute_purge_policy(
        pool: &SqlitePool,
        policy: &RetentionPolicy,
    ) -> Result<u64, String> {
        let root =
            PathBuf::from(std::env::var("ARCHIVE_DIR").unwrap_or_else(|_| "archives".into()));
        Self::execute_policy(pool, policy, &root)
            .await
            .map(|o| o.rows_deleted)
            .map_err(|e| e.to_string())
    }
}

/// Runs each policy in order, skipping tables that don't exist. One
/// failing policy doesn't stop the others.
pub async fn run_policies(
    pool: &SqlitePool,
    policies: &[RetentionPolicy],
    archive_root: &Path,
) -> Vec<(String, Result<RetentionOutcome, RetentionError>)> {
    let mut results = Vec::new();
    for p in policies {
        match table_exists(pool, &p.table_name).await {
            Ok(false) => continue,
            Err(e) => {
                results.push((p.table_name.clone(), Err(e.into())));
                continue;
            }
            Ok(true) => {}
        }
        let r = RetentionService::execute_policy(pool, p, archive_root).await;
        if let Err(e) = &r {
            tracing::error!(table = %p.table_name, error = %e, "retention policy failed");
        }
        results.push((p.table_name.clone(), r));
    }
    results
}

/// Applies `policies` every `interval`. Spawn once per deployment (or
/// behind the keeper lease so only one instance archives).
pub async fn retention_loop(
    pool: SqlitePool,
    policies: Vec<RetentionPolicy>,
    archive_root: PathBuf,
    interval: std::time::Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        run_policies(&pool, &policies, &archive_root).await;
    }
}

async fn manifest(
    pool: &SqlitePool,
    manifest_id: &str,
) -> Result<(String, String, i64, String), RetentionError> {
    sqlx::query_as(
        "SELECT table_name, file_path, row_count, checksum FROM archive_manifests WHERE id = ?1",
    )
    .bind(manifest_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| RetentionError::ManifestNotFound(manifest_id.to_string()))
}

/// Re-reads an archive file and checks it against its manifest (sha256 and
/// row count). Returns the file contents on success.
pub async fn verify_archive(
    pool: &SqlitePool,
    archive_root: &Path,
    manifest_id: &str,
) -> Result<String, RetentionError> {
    let (_, rel, row_count, checksum) = manifest(pool, manifest_id).await?;
    let body = std::fs::read_to_string(archive_root.join(&rel))?;
    let actual = format!("sha256:{}", sha256_hex(body.as_bytes()));
    if actual != checksum {
        return Err(RetentionError::ChecksumMismatch {
            manifest_id: manifest_id.to_string(),
            expected: checksum,
            actual,
        });
    }
    let lines = body.lines().count() as i64;
    if lines != row_count {
        return Err(RetentionError::ChecksumMismatch {
            manifest_id: manifest_id.to_string(),
            expected: format!("{row_count} rows"),
            actual: format!("{lines} rows"),
        });
    }
    Ok(body)
}

/// Loads an archived batch back into its table (e.g. for an audit or a
/// dispute) after verifying it. Rows already present are left untouched.
/// Returns the number of rows inserted.
pub async fn restore_archive(
    pool: &SqlitePool,
    archive_root: &Path,
    manifest_id: &str,
) -> Result<u64, RetentionError> {
    let body = verify_archive(pool, archive_root, manifest_id).await?;
    let (table, ..) = manifest(pool, manifest_id).await?;
    if !is_identifier(&table) {
        return Err(RetentionError::InvalidPolicy(format!(
            "bad table name '{table}'"
        )));
    }
    let columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info(?1) ORDER BY cid")
            .bind(&table)
            .fetch_all(pool)
            .await?;

    let values: Vec<String> = columns
        .iter()
        .map(|c| {
            let path = format!("$.{}", serde_json::to_string(c).expect("string serialises"))
                .replace('\'', "''");
            format!(
                "CASE WHEN json_type(?1, '{path}') = 'object' \
                 THEN unhex(json_extract(?1, '{path}.\"$hex\"')) \
                 ELSE json_extract(?1, '{path}') END"
            )
        })
        .collect();
    let insert = format!(
        "INSERT OR IGNORE INTO {} ({}) VALUES ({})",
        quote(&table),
        columns
            .iter()
            .map(|c| quote(c))
            .collect::<Vec<_>>()
            .join(", "),
        values.join(", ")
    );

    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let mut inserted = 0;
    for line in body.lines() {
        inserted += sqlx::query(&insert)
            .bind(line)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    }
    sqlx::query(
        "UPDATE archive_manifests SET restored_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
         WHERE id = ?1",
    )
    .bind(manifest_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(inserted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    struct Fixture {
        pool: SqlitePool,
        root: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    async fn fixture() -> Fixture {
        let root = std::env::temp_dir().join(format!("zenith-retention-{}", uuid::Uuid::new_v4()));
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                "sqlite::memory:"
                    .parse::<SqliteConnectOptions>()
                    .unwrap()
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0011_archive_manifest.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(include_str!(
            "../migrations/0014_archive_manifest_restore.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(
            "CREATE TABLE chain_events (
                 id INTEGER PRIMARY KEY, stream TEXT, data BLOB, ingested_at TEXT NOT NULL);
             CREATE TABLE settled (
                 id INTEGER PRIMARY KEY,
                 event_id INTEGER NOT NULL REFERENCES chain_events (id) ON DELETE CASCADE,
                 created_at TEXT NOT NULL);
             CREATE TABLE leases_log (id INTEGER PRIMARY KEY, at INTEGER NOT NULL);",
        )
        .execute(&pool)
        .await
        .unwrap();
        Fixture { pool, root }
    }

    fn policy(table: &str, col: &str, archive: bool, batch: u32) -> RetentionPolicy {
        RetentionPolicy {
            table_name: table.into(),
            age_column: col.into(),
            retention_days: 30,
            archive,
            batch_size: batch,
        }
    }

    /// 5 old events (400 days) and 2 recent ones; one old row has a blob,
    /// one a quote in its text.
    async fn seed_events(pool: &SqlitePool) {
        for i in 1..=7 {
            let days = if i <= 5 { 400 } else { 1 };
            sqlx::query(
                "INSERT INTO chain_events (id, stream, data, ingested_at)
                 VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?4))",
            )
            .bind(i)
            .bind(if i == 2 { "it's \"quoted\"" } else { "zenith" })
            .bind(if i == 1 {
                Some(vec![0u8, 255, 16])
            } else {
                None
            })
            .bind(format!("-{days} days"))
            .execute(pool)
            .await
            .unwrap();
        }
    }

    async fn count(pool: &SqlitePool, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[test]
    fn policy_validation() {
        assert!(matches!(
            validate_policy(&policy("positions", "opened_at", true, 10)),
            Err(RetentionError::Protected(_))
        ));
        assert!(matches!(
            validate_policy(&policy("chain_events", "ingested_at", false, 10)),
            Err(RetentionError::ArchiveRequired(_))
        ));
        assert!(matches!(
            validate_policy(&policy("x; DROP TABLE accounts", "c", true, 10)),
            Err(RetentionError::InvalidPolicy(_))
        ));
        assert!(matches!(
            validate_policy(&policy("t", "c", true, 0)),
            Err(RetentionError::InvalidPolicy(_))
        ));
        assert!(matches!(
            validate_policy(&policy("t", "c", true, MAX_BATCH_SIZE + 1)),
            Err(RetentionError::InvalidPolicy(_))
        ));
        for p in default_policies() {
            validate_policy(&p).unwrap();
        }
    }

    #[test]
    fn protected_error_message_is_stable() {
        let e = RetentionError::Protected("accounts".into()).to_string();
        assert!(e.contains("protected denylist"));
    }

    #[tokio::test]
    async fn archives_in_batches_then_deletes_only_expired_rows() {
        let f = fixture().await;
        seed_events(&f.pool).await;

        let out = RetentionService::execute_policy(
            &f.pool,
            &policy("chain_events", "ingested_at", true, 2),
            &f.root,
        )
        .await
        .unwrap();
        assert_eq!(out.rows_deleted, 5);
        assert_eq!(out.rows_archived, 5);
        assert_eq!(out.batches, 3);
        assert_eq!(out.manifest_ids.len(), 3);
        assert_eq!(count(&f.pool, "chain_events").await, 2);

        let (rows, checksum): (i64, String) = sqlx::query_as(
            "SELECT SUM(row_count), MIN(checksum) FROM archive_manifests WHERE table_name = 'chain_events'",
        )
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(rows, 5);
        assert!(checksum.starts_with("sha256:") && checksum.len() == 7 + 64);
        for id in &out.manifest_ids {
            verify_archive(&f.pool, &f.root, id).await.unwrap();
        }

        // Second run is a no-op.
        let again = RetentionService::execute_policy(
            &f.pool,
            &policy("chain_events", "ingested_at", true, 2),
            &f.root,
        )
        .await
        .unwrap();
        assert_eq!(again.rows_deleted, 0);
    }

    #[tokio::test]
    async fn archive_round_trips_exactly_including_blobs_and_quotes() {
        let f = fixture().await;
        seed_events(&f.pool).await;
        let before: Vec<(i64, String, Option<Vec<u8>>, String)> =
            sqlx::query_as("SELECT * FROM chain_events WHERE id <= 5 ORDER BY id")
                .fetch_all(&f.pool)
                .await
                .unwrap();

        let out = RetentionService::execute_policy(
            &f.pool,
            &policy("chain_events", "ingested_at", true, 100),
            &f.root,
        )
        .await
        .unwrap();
        let restored = restore_archive(&f.pool, &f.root, &out.manifest_ids[0])
            .await
            .unwrap();
        assert_eq!(restored, 5);

        let after: Vec<(i64, String, Option<Vec<u8>>, String)> =
            sqlx::query_as("SELECT * FROM chain_events WHERE id <= 5 ORDER BY id")
                .fetch_all(&f.pool)
                .await
                .unwrap();
        assert_eq!(before, after);
        let restored_at: Option<String> =
            sqlx::query_scalar("SELECT restored_at FROM archive_manifests WHERE id = ?1")
                .bind(&out.manifest_ids[0])
                .fetch_one(&f.pool)
                .await
                .unwrap();
        assert!(restored_at.is_some());
    }

    #[tokio::test]
    async fn tampered_archive_fails_verification() {
        let f = fixture().await;
        seed_events(&f.pool).await;
        let out = RetentionService::execute_policy(
            &f.pool,
            &policy("chain_events", "ingested_at", true, 100),
            &f.root,
        )
        .await
        .unwrap();
        let rel: String = sqlx::query_scalar("SELECT file_path FROM archive_manifests")
            .fetch_one(&f.pool)
            .await
            .unwrap();
        let path = f.root.join(rel);
        let tampered = std::fs::read_to_string(&path)
            .unwrap()
            .replace("zenith", "zenitH");
        std::fs::write(&path, tampered).unwrap();
        assert!(matches!(
            verify_archive(&f.pool, &f.root, &out.manifest_ids[0]).await,
            Err(RetentionError::ChecksumMismatch { .. })
        ));
        assert!(restore_archive(&f.pool, &f.root, &out.manifest_ids[0])
            .await
            .is_err());
    }

    #[tokio::test]
    async fn referenced_parent_rows_are_not_deleted_until_children_are_archived() {
        let f = fixture().await;
        seed_events(&f.pool).await;
        sqlx::query(
            "INSERT INTO settled (event_id, created_at)
             VALUES (1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-400 days'))",
        )
        .execute(&f.pool)
        .await
        .unwrap();

        let blocked = RetentionService::execute_policy(
            &f.pool,
            &policy("chain_events", "ingested_at", true, 100),
            &f.root,
        )
        .await;
        assert!(
            matches!(blocked, Err(RetentionError::Referenced { ref child, rows: 1, .. }) if child == "settled")
        );
        // Nothing was deleted, no manifest written, the cascade never fired.
        assert_eq!(count(&f.pool, "chain_events").await, 7);
        assert_eq!(count(&f.pool, "settled").await, 1);
        assert_eq!(count(&f.pool, "archive_manifests").await, 0);

        // Children first, then the parent succeeds.
        let results = run_policies(
            &f.pool,
            &[
                policy("settled", "created_at", true, 100),
                policy("chain_events", "ingested_at", true, 100),
                policy("does_not_exist", "created_at", true, 100),
            ],
            &f.root,
        )
        .await;
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|(_, r)| r.is_ok()));
        assert_eq!(count(&f.pool, "chain_events").await, 2);
    }

    #[tokio::test]
    async fn integer_epoch_age_columns_and_purge_without_archive() {
        let f = fixture().await;
        sqlx::raw_sql(
            "INSERT INTO leases_log (at) VALUES
                 (CAST(strftime('%s', 'now', '-90 days') AS INTEGER)),
                 (CAST(strftime('%s', 'now', '-1 days') AS INTEGER));",
        )
        .execute(&f.pool)
        .await
        .unwrap();
        let out = RetentionService::execute_policy(
            &f.pool,
            &policy("leases_log", "at", false, 10),
            &f.root,
        )
        .await
        .unwrap();
        assert_eq!(out.rows_deleted, 1);
        assert_eq!(out.rows_archived, 0);
        assert_eq!(count(&f.pool, "leases_log").await, 1);
        assert_eq!(count(&f.pool, "archive_manifests").await, 0);
    }

    #[tokio::test]
    async fn unknown_tables_and_columns_are_rejected() {
        let f = fixture().await;
        for p in [
            policy("nope", "created_at", true, 10),
            policy("chain_events", "nope", true, 10),
        ] {
            assert!(matches!(
                RetentionService::execute_policy(&f.pool, &p, &f.root).await,
                Err(RetentionError::InvalidPolicy(_))
            ));
        }
    }
}
