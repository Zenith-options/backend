use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::str::FromStr;

/// Opens (creating if necessary) the SQLite database at `database_url` and
/// runs any migrations under `migrations/` that haven't been applied yet.
/// `sqlx::migrate!()` embeds the migration files at compile time, so this
/// doesn't need a live DB connection to build — only to run.
pub async fn init_pool(database_url: &str) -> SqlitePool {
    let options = SqliteConnectOptions::from_str(database_url)
        .expect("invalid DATABASE_URL")
        .create_if_missing(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .expect("failed to connect to sqlite database");

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("failed to run database migrations");

    pool
}
