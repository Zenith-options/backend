//! Helpers for the chain integration tests (fees, footprint, indexer).
//!
//! The tests open a throwaway SQLite database and apply the *real* migration
//! files, so the schema the code expects and the schema the migrations
//! create can never drift apart.

use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;
use std::path::PathBuf;

/// The migration that creates the indexer tables.
pub const INDEXER_SCHEMA: &str = include_str!("../../migrations/0012_indexed_ledger_ranges.sql");

/// Open a throwaway database with the indexer schema applied.
pub async fn indexer_pool(name: &str) -> (SqlitePool, PathBuf) {
    let path = std::env::temp_dir().join(format!("zenith-{name}-{}.db", uuid::Uuid::new_v4()));
    let url = format!("sqlite://{}", path.display());
    let options = zenith_backend::db::tuned_sqlite_options(&url);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("connect to the test database");

    apply_schema(&pool, INDEXER_SCHEMA).await;
    (pool, path)
}

/// Apply a migration file to a pool, statement by statement.
pub async fn apply_schema(pool: &SqlitePool, schema: &str) {
    for statement in split_statements(schema) {
        sqlx::query(&statement)
            .execute(pool)
            .await
            .unwrap_or_else(|err| panic!("failed to apply `{statement}`: {err}"));
    }
}

/// Split a migration file into its statements.
///
/// Comment lines are dropped first: a comment may contain a semicolon, and
/// splitting on it would cut a statement in half.
fn split_statements(schema: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();

    for line in schema.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("--") {
            continue;
        }
        current.push_str(line);
        current.push('\n');
        if trimmed.ends_with(';') {
            statements.push(std::mem::take(&mut current));
        }
    }

    if !current.trim().is_empty() {
        statements.push(current);
    }

    statements
}

/// Remove a throwaway database.
pub fn cleanup(path: PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
    let _ = std::fs::remove_file(path.with_extension("db-shm"));
}
