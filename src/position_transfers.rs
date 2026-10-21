//! Persistence helpers for contract-confirmed option-position transfers.
//!
//! Offer creation is off-chain metadata. The caller of `apply_confirmed_transfer`
//! must be a trusted contract-event processor that has verified the transfer
//! event and settlement transaction on-chain.

use sqlx::{SqlitePool, Transaction};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TransferOffer {
    pub id: String,
    pub position_id: String,
    pub from_wallet: String,
    pub to_wallet: Option<String>,
    pub offer_type: String,
    pub asking_amount: Option<i64>,
    pub asking_asset: Option<String>,
    pub expires_at: String,
    pub status: String,
}

fn invalid(message: &'static str) -> sqlx::Error {
    sqlx::Error::Protocol(message.to_owned())
}

async fn create_offer(
    pool: &SqlitePool,
    offer_id: &str,
    position_id: &str,
    from_wallet: &str,
    to_wallet: Option<&str>,
    offer_type: &str,
    asking_amount: Option<i64>,
    asking_asset: Option<&str>,
    expires_at: &str,
) -> Result<TransferOffer, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let owner: Option<String> = sqlx::query_scalar(
        "SELECT wallet_address FROM positions
         WHERE id = ? AND status = 'open'",
    )
    .bind(position_id)
    .fetch_optional(&mut *tx)
    .await?;
    if owner.as_deref() != Some(from_wallet) {
        return Err(invalid("position is not open and owned by the offer wallet"));
    }

    let offer = sqlx::query_as::<_, TransferOffer>(
        "INSERT INTO position_transfer_offers
            (id, position_id, from_wallet, to_wallet, offer_type,
             asking_amount, asking_asset, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id, position_id, from_wallet, to_wallet, offer_type,
                   asking_amount, asking_asset, expires_at, status",
    )
    .bind(offer_id)
    .bind(position_id)
    .bind(from_wallet)
    .bind(to_wallet)
    .bind(offer_type)
    .bind(asking_amount)
    .bind(asking_asset)
    .bind(expires_at)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(offer)
}

/// Create a bilateral offer; only the named recipient may accept it.
pub async fn create_bilateral_offer(
    pool: &SqlitePool,
    offer_id: &str,
    position_id: &str,
    from_wallet: &str,
    to_wallet: &str,
    expires_at: &str,
) -> Result<TransferOffer, sqlx::Error> {
    if from_wallet == to_wallet {
        return Err(invalid("sender and recipient must differ"));
    }
    create_offer(
        pool,
        offer_id,
        position_id,
        from_wallet,
        Some(to_wallet),
        "bilateral",
        None,
        None,
        expires_at,
    )
    .await
}

/// Publish an on-chain listing mirror. The contract remains authoritative for
/// payment and ownership; accepting it requires a verified contract event.
pub async fn create_listing(
    pool: &SqlitePool,
    offer_id: &str,
    position_id: &str,
    from_wallet: &str,
    asking_amount: i64,
    asking_asset: &str,
    expires_at: &str,
) -> Result<TransferOffer, sqlx::Error> {
    if asking_amount <= 0 || asking_asset.trim().is_empty() {
        return Err(invalid("listing price and asset are required"));
    }
    create_offer(
        pool,
        offer_id,
        position_id,
        from_wallet,
        None,
        "listing",
        Some(asking_amount),
        Some(asking_asset),
        expires_at,
    )
    .await
}

/// Apply a transfer after an external trusted verifier confirms the
/// corresponding contract event. The position row (including collateral and
/// expiry obligations) is retained; collateral projections move atomically.
pub async fn apply_confirmed_transfer(
    pool: &SqlitePool,
    offer_id: &str,
    recipient_wallet: &str,
    chain_tx_hash: &str,
    confirmation_id: &str,
) -> Result<(), sqlx::Error> {
    if chain_tx_hash.trim().is_empty() {
        return Err(invalid("a verified chain transaction hash is required"));
    }
    let mut tx = pool.begin().await?;
    let offer = sqlx::query_as::<_, TransferOffer>(
        "SELECT id, position_id, from_wallet, to_wallet, offer_type,
                asking_amount, asking_asset, expires_at, status
         FROM position_transfer_offers
         WHERE id = ? AND status = 'open' AND expires_at > strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
    )
    .bind(offer_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| invalid("transfer offer is not open"))?;

    if offer.to_wallet.as_deref().is_some_and(|wallet| wallet != recipient_wallet) {
        return Err(invalid("recipient does not match bilateral offer"));
    }
    if offer.from_wallet == recipient_wallet {
        return Err(invalid("sender and recipient must differ"));
    }

    let position = sqlx::query_as::<_, (String, i64)>(
        "SELECT wallet_address, collateral FROM positions
         WHERE id = ? AND status = 'open'",
    )
    .bind(&offer.position_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| invalid("position is no longer open"))?;
    if position.0 != offer.from_wallet {
        return Err(invalid("position owner changed after offer creation"));
    }

    let claimed = sqlx::query(
        "UPDATE position_transfer_offers SET status = 'accepted'
         WHERE id = ? AND status = 'open'",
    )
    .bind(offer_id)
    .execute(&mut *tx)
    .await?;
    if claimed.rows_affected() != 1 {
        return Err(invalid("transfer offer was already consumed"));
    }

    sqlx::query(
        "INSERT INTO position_transfer_confirmations
            (id, offer_id, position_id, from_wallet, to_wallet, chain_tx_hash, confirmed_at)
         VALUES (?, ?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
    )
    .bind(confirmation_id)
    .bind(offer_id)
    .bind(&offer.position_id)
    .bind(&offer.from_wallet)
    .bind(recipient_wallet)
    .bind(chain_tx_hash)
    .execute(&mut *tx)
    .await?;

    let moved = sqlx::query(
        "UPDATE positions SET wallet_address = ?
         WHERE id = ? AND wallet_address = ? AND status = 'open'",
    )
    .bind(recipient_wallet)
    .bind(&offer.position_id)
    .bind(&offer.from_wallet)
    .execute(&mut *tx)
    .await?;
    if moved.rows_affected() != 1 {
        return Err(invalid("position ownership changed during transfer"));
    }

    let debited = sqlx::query(
        "UPDATE accounts SET collateral_locked = collateral_locked - ?
         WHERE wallet_address = ? AND collateral_locked >= ?",
    )
    .bind(position.1)
    .bind(&offer.from_wallet)
    .bind(position.1)
    .execute(&mut *tx)
    .await?;
    if debited.rows_affected() != 1 {
        return Err(invalid("source collateral projection is inconsistent"));
    }
    let credited = sqlx::query(
        "UPDATE accounts SET collateral_locked = collateral_locked + ?
         WHERE wallet_address = ?",
    )
    .bind(position.1)
    .bind(recipient_wallet)
    .execute(&mut *tx)
    .await?;
    if credited.rows_affected() != 1 {
        return Err(invalid("recipient account does not exist"));
    }

    tx.commit().await
}
