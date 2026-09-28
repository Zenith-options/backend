//! Collateral requirements for writing (selling) options, ported bit-for-bit
//! from the frontend's `lib/collateral.ts`: covered calls are 100% covered
//! by the underlying's current value, cash-secured puts are
//! over-collateralized by 110% of the strike (protects against a further
//! drop before the writer can react). Only applies to the short/write
//! side — buying an option never requires collateral, just the premium.

pub fn collateral_required(option_type: &str, contracts: f64, strike: f64, spot: f64) -> f64 {
    if option_type == "call" {
        contracts * spot
    } else {
        contracts * strike * 1.1
    }
}

/// Returns `(wallet_address, collateral_locked, open_collateral_sum)` for every
/// wallet whose `collateral_locked` has drifted from the sum of its open
/// positions' collateral. An empty result means every wallet's locked
/// collateral is fully accounted for by its open positions.
///
/// This is a defence-in-depth check job, not a request-path query: the
/// application maintains the sum in the same transaction as every open/close/
/// roll, so a non-empty result means something outside that path (a manual SQL
/// fix, a future service writing to the same database) corrupted the
/// invariant. It uses a runtime query rather than a checked macro because it
/// is a diagnostic that runs on demand, not on the request path.
pub async fn check_collateral_sum(
    db: &sqlx::SqlitePool,
) -> Result<Vec<(String, f64, f64)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT a.wallet_address, a.collateral_locked, COALESCE(SUM(p.collateral), 0.0)
           FROM accounts a
           LEFT JOIN positions p ON p.wallet_address = a.wallet_address AND p.status = 'open'
          GROUP BY a.wallet_address, a.collateral_locked
         HAVING a.collateral_locked != COALESCE(SUM(p.collateral), 0.0)",
    )
    .fetch_all(db)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covered_call_is_100_percent_of_spot() {
        assert_eq!(
            collateral_required("call", 2.0, 70000.0, 67420.50),
            2.0 * 67420.50
        );
    }

    #[test]
    fn cash_secured_put_is_110_percent_of_strike() {
        assert_eq!(
            collateral_required("put", 3.0, 60000.0, 67420.50),
            3.0 * 60000.0 * 1.1
        );
    }

    #[tokio::test]
    async fn check_collateral_sum_flags_a_drifted_wallet() {
        let db_path =
            std::env::temp_dir().join(format!("zenith-collateral-test-{}.db", uuid::Uuid::new_v4()));
        let pool = crate::db::init_pool(&format!("sqlite://{}", db_path.display())).await;

        sqlx::query("INSERT INTO accounts (wallet_address, collateral_locked) VALUES ('GTEST', 100)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO positions
                (id, wallet_address, underlying, strike, expiry_days, option_type,
                 position_type, contracts, entry_premium, entry_spot, collateral, status)
             VALUES ('p1', 'GTEST', 'BTC', 70000, 30, 'call', 'short', 1, 100, 67000, 100, 'open')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Locked collateral (100) matches the open position's collateral (100).
        let drift = check_collateral_sum(&pool).await.unwrap();
        assert!(drift.is_empty(), "no drift expected: {drift:?}");

        // Simulate corruption outside the app's transactional open/close path.
        sqlx::query("UPDATE accounts SET collateral_locked = 50 WHERE wallet_address = 'GTEST'")
            .execute(&pool)
            .await
            .unwrap();

        let drift = check_collateral_sum(&pool).await.unwrap();
        assert_eq!(drift.len(), 1, "the drifted wallet must be reported");
        let (wallet, locked, sum) = &drift[0];
        assert_eq!(wallet, "GTEST");
        assert_eq!(*locked, 50.0);
        assert_eq!(*sum, 100.0);

        pool.close().await;
        let _ = std::fs::remove_file(&db_path);
    }
}
