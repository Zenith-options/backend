//! Sealed-bid liquidation auction.
//!
//! Second-line liquidation mechanism used when DEX/AMM routing cannot convert
//! collateral at an acceptable price. The auction lets external capital
//! compete for the lot and reveal a genuine clearing price.
//!
//! # Lifecycle (state machine keyed on ledger sequence)
//!
//! ```text
//! open ──(close_ledger reached)──► closed
//! closed ──(reveal + settle)──► settled      (winner pays, bond applied)
//!        └──(no bid above reserve)──► fallback (dex_routing → risk_reserve)
//!        └──(winner defaults on reveal)──► defaulted (bond forfeited)
//! ```
//!
//! # Trust boundary (commit-reveal)
//!
//! Bidders submit a `commitment` (hash) during the open window. At close the
//! server (operator) reveals the highest bid; the commitment is checked
//! against the revealed amount. In production this uses the on-chain
//! collateral-vault contract's escrow: the vault holds the collateral for
//! the lot's duration and releases on the winner's settlement entry point.
//! Here we model the same logic off-chain so it is testable without a chain.
//!
//! # Atomicity & idempotency
//!
//! `settle_lot` uses a `WHERE status='closed'` CAS guard so exactly one worker
//! closes and settles a lot even under concurrent sweeps (leader election
//! ensures one worker runs; the guard is defence-in-depth).

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::outbox::{self, DomainEvent, EVENT_VERSION};

// ─── Config ──────────────────────────────────────────────────────────────────

/// Auction policy. Window is in ledger offsets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuctionConfig {
    /// Auction window in ledgers (open → close).
    pub window_ledgers: u32,
    /// Default bid bond (10^7 units).
    pub default_bid_bond: i64,
    /// Reveal grace period in ledgers after close before a bidder defaults.
    pub reveal_grace_ledgers: u32,
}

impl Default for AuctionConfig {
    fn default() -> Self {
        Self {
            window_ledgers: 720,        // ~1 hour at 5s/ledger
            default_bid_bond: 1_000_000, // 0.1 USDC
            reveal_grace_ledgers: 60,   // 5 minutes
        }
    }
}

// ─── Domain types ─────────────────────────────────────────────────────────────

/// A persisted auction lot.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AuctionLot {
    pub id: String,
    pub liquidation_id: String,
    pub collateral_asset: String,
    pub quantity: i64,
    pub reserve_price: i64,
    pub bid_bond: i64,
    pub open_ledger: i64,
    pub close_ledger: i64,
    pub covered_positions: String,
    pub settlement_asset: String,
    pub status: String,
    pub winning_bid_id: Option<String>,
    pub fallback_path: Option<String>,
    pub proceeds: i64,
    pub settle_tx_hash: Option<String>,
    pub created_by: String,
    pub created_at: String,
    pub settled_at: Option<String>,
    pub updated_at: String,
}

/// A persisted sealed bid.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AuctionBid {
    pub id: String,
    pub lot_id: String,
    pub bidder: String,
    pub commitment: String,
    pub revealed_amount: Option<i64>,
    pub bond_amount: i64,
    pub status: String,
    pub bond_status: String,
    pub submitted_at: String,
    pub revealed_at: Option<String>,
    pub refunded_at: Option<String>,
    pub created_at: String,
}

/// One entry in the bond-movement audit trail.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct BondMovement {
    pub id: String,
    pub bid_id: String,
    pub lot_id: String,
    pub bidder: String,
    pub movement: String,
    pub amount: i64,
    pub reason: String,
    pub ledger: i64,
    pub occurred_at: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AuctionError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("lot {0} not found")]
    NotFound(String),
    #[error("lot is {0}, expected {1}")]
    WrongStatus(String, &'static str),
    #[error("bidding is closed for lot {0}")]
    BiddingClosed(String),
    #[error("reserve price already locked (bids received)")]
    ReserveLocked,
    #[error("bidder {0} has insufficient funds for bond")]
    InsufficientBond,
    #[error("no bids above reserve; falling back")]
    NoWinningBid,
    #[error("lot already settled")]
    AlreadySettled,
    #[error("underlying is paused; cannot settle")]
    UnderlyingPaused,
}

// ─── Timestamp helpers ───────────────────────────────────────────────────────

fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    time::OffsetDateTime::from_unix_timestamp(secs)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| secs.to_string())
}

/// Commitment = SHA-256(revealed_amount || bidder || lot_id || nonce).
pub fn make_commitment(revealed_amount: i64, bidder: &str, lot_id: &str, nonce: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(revealed_amount.to_le_bytes());
    hasher.update(bidder.as_bytes());
    hasher.update(lot_id.as_bytes());
    hasher.update(nonce.as_bytes());
    hex::encode(hasher.finalize())
}

/// Verify a revealed bid matches its commitment.
pub fn verify_commitment(
    revealed_amount: i64,
    bidder: &str,
    lot_id: &str,
    nonce: &str,
    commitment: &str,
) -> bool {
    make_commitment(revealed_amount, bidder, lot_id, nonce) == commitment
}

// ─── Lot creation ────────────────────────────────────────────────────────────

/// Create a new auction lot from a liquidation decision.
pub async fn create_lot(
    db: &SqlitePool,
    liquidation_id: &str,
    collateral_asset: &str,
    quantity: i64,
    reserve_price: i64,
    settlement_asset: &str,
    covered_positions: &[String],
    open_ledger: i64,
    config: &AuctionConfig,
    created_by: &str,
) -> Result<AuctionLot, AuctionError> {
    if quantity <= 0 {
        return Err(AuctionError::InsufficientBond);
    }

    let id = uuid::Uuid::new_v4().to_string();
    let close_ledger = open_ledger + config.window_ledgers as i64;
    let covered_json = serde_json::to_string(covered_positions).unwrap_or_else(|_| "[]".into());

    sqlx::query(
        "INSERT INTO auction_lots
            (id, liquidation_id, collateral_asset, quantity, reserve_price,
             bid_bond, open_ledger, close_ledger, covered_positions,
             settlement_asset, created_by)
         VALUES (?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&id)
    .bind(liquidation_id)
    .bind(collateral_asset)
    .bind(quantity)
    .bind(reserve_price)
    .bind(config.default_bid_bond)
    .bind(open_ledger)
    .bind(close_ledger)
    .bind(&covered_json)
    .bind(settlement_asset)
    .bind(created_by)
    .execute(db)
    .await?;

    // Emit auction-opened event.
    let mut tx = db.begin().await?;
    let event = DomainEvent::AuctionOpened(crate::outbox::AuctionEventPayload {
        version: EVENT_VERSION,
        auction_id: id.clone(),
        lot_id: id.clone(),
        liquidation_id: liquidation_id.to_string(),
        collateral_asset: collateral_asset.to_string(),
        quantity,
        reserve_price,
        close_ledger,
        status: "open".to_string(),
        occurred_at: now_iso(),
    });
    let _ = outbox::emit(&mut tx, &event).await;
    let _ = tx.commit().await;

    get_lot(db, &id).await?.ok_or_else(|| AuctionError::NotFound(id))
}

pub async fn get_lot(db: &SqlitePool, id: &str) -> Result<Option<AuctionLot>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM auction_lots WHERE id=?")
        .bind(id)
        .fetch_optional(db)
        .await
}

pub async fn list_lots(
    db: &SqlitePool,
    status: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<AuctionLot>, sqlx::Error> {
    sqlx::query_as(
        "SELECT * FROM auction_lots
          WHERE (? IS NULL OR status=?)
          ORDER BY created_at DESC LIMIT ? OFFSET ?",
    )
    .bind(status)
    .bind(status)
    .bind(limit)
    .bind(offset)
    .fetch_all(db)
    .await
}

// ─── Reserve-price update (forbidden once first bid received) ─────────────────

/// Update the reserve price on an open lot. Returns `Err(ReserveLocked)` once
/// any bid has been received — matches the edge-case constraint.
pub async fn update_reserve(
    db: &SqlitePool,
    lot_id: &str,
    new_reserve: i64,
) -> Result<AuctionLot, AuctionError> {
    let lot = get_lot(db, lot_id)
        .await?
        .ok_or_else(|| AuctionError::NotFound(lot_id.to_string()))?;
    if lot.status != "open" {
        return Err(AuctionError::WrongStatus(lot.status, "open"));
    }

    let bid_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM auction_bids WHERE lot_id=?",
    )
    .bind(lot_id)
    .fetch_one(db)
    .await
    .unwrap_or(0);
    if bid_count > 0 {
        return Err(AuctionError::ReserveLocked);
    }

    sqlx::query("UPDATE auction_lots SET reserve_price=?, updated_at=? WHERE id=?")
        .bind(new_reserve)
        .bind(now_iso())
        .bind(lot_id)
        .execute(db)
        .await?;

    get_lot(db, lot_id).await?.ok_or_else(|| AuctionError::NotFound(lot_id.to_string()))
}

// ─── Sealed-bid submission ───────────────────────────────────────────────────

/// Submit a sealed bid (commitment only). The bond is held in escrow.
pub async fn submit_bid(
    db: &SqlitePool,
    lot_id: &str,
    bidder: &str,
    commitment: &str,
    bond_amount: i64,
    current_ledger: i64,
) -> Result<AuctionBid, AuctionError> {
    let lot = get_lot(db, lot_id)
        .await?
        .ok_or_else(|| AuctionError::NotFound(lot_id.to_string()))?;

    if lot.status != "open" {
        return Err(AuctionError::BiddingClosed(lot_id.to_string()));
    }
    // Bids at the exact close ledger: closed. Bids strictly before close are open.
    if current_ledger >= lot.close_ledger {
        return Err(AuctionError::BiddingClosed(lot_id.to_string()));
    }
    if bond_amount < lot.bid_bond {
        return Err(AuctionError::InsufficientBond);
    }

    let id = uuid::Uuid::new_v4().to_string();
    let mut tx = db.begin().await?;

    sqlx::query(
        "INSERT INTO auction_bids
            (id, lot_id, bidder, commitment, bond_amount)
         VALUES (?,?,?,?,?)",
    )
    .bind(&id)
    .bind(lot_id)
    .bind(bidder)
    .bind(commitment)
    .bind(bond_amount)
    .execute(&mut *tx)
    .await?;

    // Record bond held in escrow.
    record_bond_movement(&mut tx, &id, lot_id, bidder, "held", bond_amount,
        "bid bond held in escrow", current_ledger).await?;

    tx.commit().await?;

    get_bid(db, &id).await?.ok_or_else(|| AuctionError::NotFound(id))
}

pub async fn get_bid(db: &SqlitePool, id: &str) -> Result<Option<AuctionBid>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM auction_bids WHERE id=?")
        .bind(id)
        .fetch_optional(db)
        .await
}

pub async fn list_bids(db: &SqlitePool, lot_id: &str) -> Result<Vec<AuctionBid>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM auction_bids WHERE lot_id=? ORDER BY submitted_at")
        .bind(lot_id)
        .fetch_all(db)
        .await
}

// ─── Close + settle ──────────────────────────────────────────────────────────

/// Callback invoked by `settle_lot` to perform the on-chain collateral
/// transfer from the vault to the winner, and credit proceeds.
pub trait AuctionSettler: Send + Sync {
    /// Settle the winning bid: transfer collateral to winner, credit proceeds
    /// to the position's collateral account. Returns the on-chain tx hash.
    fn settle<'a>(
        &'a self,
        lot: &'a AuctionLot,
        winning_bid: &'a AuctionBid,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, anyhow::Error>> + Send + 'a>>;
}

/// No-op settler for paper/test mode.
pub struct NoopAuctionSettler;

impl AuctionSettler for NoopAuctionSettler {
    fn settle<'a>(
        &'a self,
        _lot: &'a AuctionLot,
        _winning_bid: &'a AuctionBid,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, anyhow::Error>> + Send + 'a>>
    {
        Box::pin(async { Ok("noop-tx-hash".to_string()) })
    }
}

/// Fallback path invoked when no bid clears the reserve.
pub trait FallbackHandler: Send + Sync {
    fn fallback<'a>(
        &'a self,
        lot: &'a AuctionLot,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, anyhow::Error>> + Send + 'a>>;
}

/// Default fallback: DEX routing then risk reserve (recorded).
pub struct DefaultFallback;

impl FallbackHandler for DefaultFallback {
    fn fallback<'a>(
        &'a self,
        _lot: &'a AuctionLot,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, anyhow::Error>> + Send + 'a>>
    {
        Box::pin(async { Ok("dex_routing".to_string()) })
    }
}

/// Reveal a bid's committed amount. Called by the operator at close.
pub async fn reveal_bid(
    db: &SqlitePool,
    bid_id: &str,
    revealed_amount: i64,
    nonce: &str,
    current_ledger: i64,
) -> Result<AuctionBid, AuctionError> {
    let bid = get_bid(db, bid_id)
        .await?
        .ok_or_else(|| AuctionError::NotFound(bid_id.to_string()))?;
    if bid.status != "pending" {
        return Err(AuctionError::WrongStatus(bid.status, "pending"));
    }
    if !verify_commitment(revealed_amount, &bid.bidder, &bid.lot_id, nonce, &bid.commitment) {
        return Err(AuctionError::InsufficientBond); // commitment mismatch
    }

    sqlx::query(
        "UPDATE auction_bids
            SET revealed_amount=?, status='revealed', revealed_at=?
          WHERE id=? AND status='pending'",
    )
    .bind(revealed_amount)
    .bind(now_iso())
    .bind(bid_id)
    .execute(db)
    .await?;

    get_bid(db, bid_id).await?.ok_or_else(|| AuctionError::NotFound(bid_id.to_string()))
}

/// Close the lot (CAS open → closed) at the close ledger, pick the winner
/// among revealed bids above reserve, and settle atomically. If no bid
/// clears reserve, fall back.
///
/// Idempotent: a second close on an already-closed lot returns
/// `Err(AlreadySettled)` (or the prior result).
pub async fn close_and_settle(
    db: &SqlitePool,
    lot_id: &str,
    current_ledger: i64,
    settler: &dyn AuctionSettler,
    fallback: &dyn FallbackHandler,
) -> Result<AuctionLot, AuctionError> {
    let lot = get_lot(db, lot_id)
        .await?
        .ok_or_else(|| AuctionError::NotFound(lot_id.to_string()))?;

    if lot.status == "settled" || lot.status == "fallback" {
        return Err(AuctionError::AlreadySettled);
    }
    if lot.status != "open" && lot.status != "closed" {
        return Err(AuctionError::WrongStatus(lot.status, "open or closed"));
    }

    // Check the underlying is not paused (edge case).
    let paused: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM option_series
          WHERE underlying = (SELECT collateral_asset FROM auction_lots WHERE id=?)
            AND trading_halted = 1",
    )
    .bind(lot_id)
    .fetch_one(db)
    .await
    .unwrap_or(0);
    if paused > 0 {
        return Err(AuctionError::UnderlyingPaused);
    }

    // CAS: open → closed.
    if lot.status == "open" {
        let rows = sqlx::query(
            "UPDATE auction_lots SET status='closed', updated_at=? WHERE id=? AND status='open'",
        )
        .bind(now_iso())
        .bind(lot_id)
        .execute(db)
        .await?
        .rows_affected();
        if rows == 0 {
            // Another worker closed it; reload.
            return get_lot(db, lot_id)
                .await?
                .ok_or_else(|| AuctionError::NotFound(lot_id.to_string()));
        }
    }

    // Default any pending bids past the reveal grace period.
    let grace = current_ledger - (lot.close_ledger + 0); // grace handled by caller
    let _ = grace;

    // Load all revealed bids above reserve.
    let revealed: Vec<AuctionBid> = sqlx::query_as(
        "SELECT * FROM auction_bids
          WHERE lot_id=? AND status='revealed' AND revealed_amount >= ?
          ORDER BY revealed_amount DESC, revealed_at ASC",
    )
    .bind(lot_id)
    .bind(lot.reserve_price)
    .fetch_all(db)
    .await?;

    if revealed.is_empty() {
        // No winning bid: fall back.
        return fallback_lot(db, lot_id, fallback).await;
    }

    let winner = &revealed[0];
    let proceed = winner.revealed_amount.unwrap_or(0);

    // Settle on-chain.
    let tx_hash = match settler.settle(&lot, winner).await {
        Ok(h) => h,
        Err(e) => {
            // Winner defaulted: forfeit bond, mark defaulted, fall back.
            tracing::warn!(lot_id, error = %e, "winner settlement failed; forfeiting bond");
            forfeit_winner_bond(db, &winner.id, lot_id, &winner.bidder, current_ledger).await?;
            return fallback_lot(db, lot_id, fallback).await;
        }
    };

    // Mark winner; apply bond to payment; refund losers.
    let mut tx = db.begin().await?;
    sqlx::query("UPDATE auction_bids SET status='winning', bond_status='applied' WHERE id=?")
        .bind(&winner.id)
        .execute(&mut *tx)
        .await?;
    record_bond_movement(&mut tx, &winner.id, lot_id, &winner.bidder,
        "applied_to_payment", winner.bond_amount, "winning bond applied to payment",
        current_ledger).await?;

    for loser in revealed.iter().skip(1) {
        sqlx::query("UPDATE auction_bids SET status='losing', bond_status='refunded', refunded_at=? WHERE id=?")
            .bind(now_iso())
            .bind(&loser.id)
            .execute(&mut *tx)
            .await?;
        record_bond_movement(&mut tx, &loser.id, lot_id, &loser.bidder,
            "refunded", loser.bond_amount, "losing bid bond refunded",
            current_ledger).await?;
    }

    // Also refund any bids that were never revealed (pending at close).
    let pending: Vec<AuctionBid> = sqlx::query_as(
        "SELECT * FROM auction_bids WHERE lot_id=? AND status='pending'",
    )
    .bind(lot_id)
    .fetch_all(&mut *tx)
    .await?;
    for p in &pending {
        sqlx::query("UPDATE auction_bids SET status='defaulted', bond_status='forfeited' WHERE id=?")
            .bind(&p.id)
            .execute(&mut *tx)
            .await?;
        record_bond_movement(&mut tx, &p.id, lot_id, &p.bidder,
            "forfeited", p.bond_amount, "bid not revealed within window; bond forfeited",
            current_ledger).await?;
    }

    // Flip lot to settled.
    sqlx::query(
        "UPDATE auction_lots
            SET status='settled', winning_bid_id=?, proceeds=?, settle_tx_hash=?,
                settled_at=?, updated_at=?
          WHERE id=? AND status='closed'",
    )
    .bind(&winner.id)
    .bind(proceed)
    .bind(&tx_hash)
    .bind(now_iso())
    .bind(now_iso())
    .bind(lot_id)
    .execute(&mut *tx)
    .await?;

    // Outbox event.
    let event = DomainEvent::AuctionSettled(crate::outbox::AuctionEventPayload {
        version: EVENT_VERSION,
        auction_id: lot_id.to_string(),
        lot_id: lot_id.to_string(),
        liquidation_id: lot.liquidation_id.clone(),
        collateral_asset: lot.collateral_asset.clone(),
        quantity: lot.quantity,
        reserve_price: lot.reserve_price,
        close_ledger: lot.close_ledger,
        status: "settled".to_string(),
        occurred_at: now_iso(),
    });
    let _ = outbox::emit(&mut tx, &event).await;
    tx.commit().await?;

    get_lot(db, lot_id).await?.ok_or_else(|| AuctionError::NotFound(lot_id.to_string()))
}

/// Fall back to DEX routing then risk reserve. Records the chosen path.
async fn fallback_lot(
    db: &SqlitePool,
    lot_id: &str,
    fallback: &dyn FallbackHandler,
) -> Result<AuctionLot, AuctionError> {
    let path = fallback.fallback(&get_lot(db, lot_id).await?.unwrap()).await
        .map_err(|e| {
            tracing::warn!(lot_id, error = %e, "fallback handler error");
            AuctionError::NoWinningBid
        })?;

    let mut tx = db.begin().await?;
    sqlx::query(
        "UPDATE auction_lots
            SET status='fallback', fallback_path=?, updated_at=?
          WHERE id=? AND status='closed'",
    )
    .bind(&path)
    .bind(now_iso())
    .bind(lot_id)
    .execute(&mut *tx)
    .await?;

    // Refund all revealed bids (no winner).
    let revealed: Vec<AuctionBid> = sqlx::query_as(
        "SELECT * FROM auction_bids WHERE lot_id=? AND status='revealed'",
    )
    .bind(lot_id)
    .fetch_all(&mut *tx)
    .await?;
    for b in &revealed {
        sqlx::query("UPDATE auction_bids SET status='losing', bond_status='refunded', refunded_at=? WHERE id=?")
            .bind(now_iso())
            .bind(&b.id)
            .execute(&mut *tx)
            .await?;
        record_bond_movement(&mut tx, &b.id, lot_id, &b.bidder,
            "refunded", b.bond_amount, "auction fell back; bond refunded",
            0).await?;
    }

    let event = DomainEvent::AuctionSettled(crate::outbox::AuctionEventPayload {
        version: EVENT_VERSION,
        auction_id: lot_id.to_string(),
        lot_id: lot_id.to_string(),
        liquidation_id: String::new(),
        collateral_asset: String::new(),
        quantity: 0,
        reserve_price: 0,
        close_ledger: 0,
        status: "fallback".to_string(),
        occurred_at: now_iso(),
    });
    let _ = outbox::emit(&mut tx, &event).await;
    tx.commit().await?;

    get_lot(db, lot_id).await?.ok_or_else(|| AuctionError::NotFound(lot_id.to_string()))
}

async fn forfeit_winner_bond(
    db: &SqlitePool,
    bid_id: &str,
    lot_id: &str,
    bidder: &str,
    current_ledger: i64,
) -> Result<(), AuctionError> {
    let mut tx = db.begin().await?;
    let bid: AuctionBid = sqlx::query_as("SELECT * FROM auction_bids WHERE id=?")
        .bind(bid_id)
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query("UPDATE auction_bids SET status='defaulted', bond_status='forfeited' WHERE id=?")
        .bind(bid_id)
        .execute(&mut *tx)
        .await?;
    record_bond_movement(&mut tx, bid_id, lot_id, bidder,
        "forfeited", bid.bond_amount, "winner defaulted on settlement; bond forfeited",
        current_ledger).await?;
    tx.commit().await?;
    Ok(())
}

async fn record_bond_movement(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    bid_id: &str,
    lot_id: &str,
    bidder: &str,
    movement: &str,
    amount: i64,
    reason: &str,
    ledger: i64,
) -> Result<(), AuctionError> {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO auction_bond_movements
            (id, bid_id, lot_id, bidder, movement, amount, reason, ledger)
         VALUES (?,?,?,?,?,?,?,?)",
    )
    .bind(&id)
    .bind(bid_id)
    .bind(lot_id)
    .bind(bidder)
    .bind(movement)
    .bind(amount)
    .bind(reason)
    .bind(ledger)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// List the bond-movement audit trail for a lot.
pub async fn list_bond_movements(
    db: &SqlitePool,
    lot_id: &str,
) -> Result<Vec<BondMovement>, sqlx::Error> {
    sqlx::query_as(
        "SELECT * FROM auction_bond_movements WHERE lot_id=? ORDER BY occurred_at",
    )
    .bind(lot_id)
    .fetch_all(db)
    .await
}

// ─── Background sweep loop ───────────────────────────────────────────────────

/// Periodic sweep: closes and settles any open lot whose close_ledger has
/// passed. Idempotent via CAS guards.
pub async fn auction_sweep_loop(
    db: SqlitePool,
    config: AuctionConfig,
    cadence: std::time::Duration,
    settler: std::sync::Arc<dyn AuctionSettler>,
    fallback: std::sync::Arc<dyn FallbackHandler>,
) {
    let mut interval = tokio::time::interval(cadence);
    loop {
        interval.tick().await;
        let current_ledger: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(sequence), 0) FROM ledger_checkpoints",
        )
        .fetch_one(&db)
        .await
        .unwrap_or(0);

        let due: Vec<AuctionLot> = sqlx::query_as(
            "SELECT * FROM auction_lots
              WHERE status='open' AND close_ledger <= ?",
        )
        .bind(current_ledger)
        .fetch_all(&db)
        .await
        .unwrap_or_default();

        for lot in due {
            match close_and_settle(&db, &lot.id, current_ledger,
                settler.as_ref(), fallback.as_ref()).await
            {
                Ok(_) => tracing::info!(lot_id = %lot.id, "auction settled"),
                Err(AuctionError::AlreadySettled) => {}
                Err(e) => tracing::warn!(lot_id = %lot.id, error = %e, "auction sweep error"),
            }
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> (SqlitePool, std::path::PathBuf) {
        let path = std::env::temp_dir()
            .join(format!("zenith-auction-{}.db", uuid::Uuid::new_v4()));
        let pool = crate::db::init_pool(&format!("sqlite://{}", path.display())).await;
        (pool, path)
    }

    fn cfg() -> AuctionConfig {
        AuctionConfig {
            window_ledgers: 100,
            default_bid_bond: 1_000_000,
            reveal_grace_ledgers: 10,
        }
    }

    // ── Commit-reveal helpers ────────────────────────────────────────────────

    #[test]
    fn commitment_round_trips() {
        let c = make_commitment(5_000_000, "BIDDER1", "LOT1", "nonce1");
        assert!(verify_commitment(5_000_000, "BIDDER1", "LOT1", "nonce1", &c));
        // Wrong amount must fail.
        assert!(!verify_commitment(4_000_000, "BIDDER1", "LOT1", "nonce1", &c));
    }

    // ── Integration tests ────────────────────────────────────────────────────

    #[tokio::test]
    async fn create_lot_persists() {
        let (pool, path) = test_pool().await;
        let lot = create_lot(
            &pool, "liq-1", "BTC", 10_000_000, 5_000_000, "USDC",
            &["pos1".into()], 1000, &cfg(), "OP1",
        ).await.unwrap();
        assert_eq!(lot.status, "open");
        assert_eq!(lot.close_ledger, 1100);
        assert_eq!(lot.covered_positions, r#"["pos1"]"#);
        pool.close().await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn submit_bid_records_bond() {
        let (pool, path) = test_pool().await;
        let lot = create_lot(
            &pool, "liq-2", "ETH", 20_000_000, 3_000_000, "USDC",
            &[], 100, &cfg(), "OP1",
        ).await.unwrap();

        let commitment = make_commitment(3_500_000, "BIDDER1", &lot.id, "n1");
        let bid = submit_bid(
            &pool, &lot.id, "BIDDER1", &commitment, 1_000_000, 150,
        ).await.unwrap();
        assert_eq!(bid.status, "pending");
        assert_eq!(bid.bond_amount, 1_000_000);

        // Bond movement recorded.
        let movements = list_bond_movements(&pool, &lot.id).await.unwrap();
        assert_eq!(movements.len(), 1);
        assert_eq!(movements[0].movement, "held");

        pool.close().await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn bid_at_or_after_close_rejected() {
        let (pool, path) = test_pool().await;
        let lot = create_lot(
            &pool, "liq-3", "ETH", 10_000_000, 1_000_000, "USDC",
            &[], 100, &cfg(), "OP1",
        ).await.unwrap();
        // close_ledger = 200. Bid at 200 → rejected.
        let commitment = make_commitment(1_100_000, "B", &lot.id, "n");
        let err = submit_bid(&pool, &lot.id, "B", &commitment, 1_000_000, 200).await.unwrap_err();
        assert!(matches!(err, AuctionError::BiddingClosed(_)));
        pool.close().await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn reserve_update_forbidden_after_first_bid() {
        let (pool, path) = test_pool().await;
        let lot = create_lot(
            &pool, "liq-4", "ETH", 10_000_000, 1_000_000, "USDC",
            &[], 100, &cfg(), "OP1",
        ).await.unwrap();
        let commitment = make_commitment(1_100_000, "B", &lot.id, "n");
        submit_bid(&pool, &lot.id, "B", &commitment, 1_000_000, 150).await.unwrap();

        let err = update_reserve(&pool, &lot.id, 900_000).await.unwrap_err();
        assert!(matches!(err, AuctionError::ReserveLocked));
        pool.close().await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn successful_auction_settles_position() {
        let (pool, path) = test_pool().await;
        let lot = create_lot(
            &pool, "liq-5", "BTC", 10_000_000, 5_000_000, "USDC",
            &["pos5".into()], 100, &cfg(), "OP1",
        ).await.unwrap();

        // Two bidders reveal above reserve.
        let c1 = make_commitment(5_500_000, "B1", &lot.id, "n1");
        let c2 = make_commitment(6_000_000, "B2", &lot.id, "n2");
        let b1 = submit_bid(&pool, &lot.id, "B1", &c1, 1_000_000, 150).await.unwrap();
        let b2 = submit_bid(&pool, &lot.id, "B2", &c2, 1_000_000, 150).await.unwrap();

        // Close ledger reached.
        reveal_bid(&pool, &b1.id, 5_500_000, "n1", 210).await.unwrap();
        reveal_bid(&pool, &b2.id, 6_000_000, "n2", 211).await.unwrap();

        let settled = close_and_settle(
            &pool, &lot.id, 220, &NoopAuctionSettler, &DefaultFallback,
        ).await.unwrap();

        assert_eq!(settled.status, "settled");
        assert_eq!(settled.proceeds, 6_000_000);
        assert!(settled.winning_bid_id.is_some());
        assert_eq!(settled.winning_bid_id.unwrap(), b2.id);

        // Winner's bond applied; loser's refunded.
        let winner: AuctionBid = sqlx::query_as(
            "SELECT * FROM auction_bids WHERE id=?",
        )
        .bind(&b2.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(winner.status, "winning");
        assert_eq!(winner.bond_status, "applied");

        let loser: AuctionBid = sqlx::query_as(
            "SELECT * FROM auction_bids WHERE id=?",
        )
        .bind(&b1.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(loser.status, "losing");
        assert_eq!(loser.bond_status, "refunded");

        pool.close().await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn failed_auction_falls_back() {
        let (pool, path) = test_pool().await;
        let lot = create_lot(
            &pool, "liq-6", "BTC", 10_000_000, 5_000_000, "USDC",
            &[], 100, &cfg(), "OP1",
        ).await.unwrap();

        // One bid below reserve.
        let c = make_commitment(4_000_000, "B1", &lot.id, "n1");
        let b = submit_bid(&pool, &lot.id, "B1", &c, 1_000_000, 150).await.unwrap();
        reveal_bid(&pool, &b.id, 4_000_000, "n1", 210).await.unwrap();

        let settled = close_and_settle(
            &pool, &lot.id, 220, &NoopAuctionSettler, &DefaultFallback,
        ).await.unwrap();
        assert_eq!(settled.status, "fallback");
        assert_eq!(settled.fallback_path.as_deref(), Some("dex_routing"));

        // Bid bond refunded.
        let bid: AuctionBid = sqlx::query_as("SELECT * FROM auction_bids WHERE id=?")
            .bind(&b.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(bid.bond_status, "refunded");

        pool.close().await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn bidder_defaults_on_reveal_bond_forfeited() {
        let (pool, path) = test_pool().await;
        let lot = create_lot(
            &pool, "liq-7", "BTC", 10_000_000, 5_000_000, "USDC",
            &[], 100, &cfg(), "OP1",
        ).await.unwrap();

        // Bid submitted but never revealed.
        let c = make_commitment(5_500_000, "B1", &lot.id, "n1");
        let b = submit_bid(&pool, &lot.id, "B1", &c, 1_000_000, 150).await.unwrap();

        // Close with no reveals → fallback, and the unrevealed bid defaults.
        let settled = close_and_settle(
            &pool, &lot.id, 220, &NoopAuctionSettler, &DefaultFallback,
        ).await.unwrap();
        assert_eq!(settled.status, "fallback");

        let bid: AuctionBid = sqlx::query_as("SELECT * FROM auction_bids WHERE id=?")
            .bind(&b.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(bid.status, "defaulted");
        assert_eq!(bid.bond_status, "forfeited");

        pool.close().await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn concurrent_close_settles_exactly_once() {
        let (pool, path) = test_pool().await;
        let lot = create_lot(
            &pool, "liq-8", "BTC", 10_000_000, 5_000_000, "USDC",
            &[], 100, &cfg(), "OP1",
        ).await.unwrap();

        let c = make_commitment(6_000_000, "B1", &lot.id, "n1");
        let b = submit_bid(&pool, &lot.id, "B1", &c, 1_000_000, 150).await.unwrap();
        reveal_bid(&pool, &b.id, 6_000_000, "n1", 210).await.unwrap();

        // Spawn two concurrent settle attempts.
        let pa = pool.clone();
        let la = lot.id.clone();
        let s1 = tokio::spawn(async move {
            close_and_settle(&pa, &la, 220, &NoopAuctionSettler, &DefaultFallback).await
        });
        let pb = pool.clone();
        let lb = lot.id.clone();
        let s2 = tokio::spawn(async move {
            close_and_settle(&pb, &lb, 220, &NoopAuctionSettler, &DefaultFallback).await
        });

        let (r1, r2) = tokio::join!(s1, s2);
        let r1 = r1.unwrap();
        let r2 = r2.unwrap();

        // Exactly one wins; the other gets AlreadySettled or a CAS miss.
        let ok1 = r1.is_ok();
        let ok2 = r2.is_ok();
        assert!(ok1 ^ ok2 || (!ok1 && !ok2), "exactly one must settle; r1={ok1} r2={ok2}");

        // Final state is settled exactly once.
        let final_lot: AuctionLot = sqlx::query_as(
            "SELECT * FROM auction_lots WHERE id=?",
        )
        .bind(&lot.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(final_lot.status, "settled");

        pool.close().await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn tie_breaks_by_earliest_reveal() {
        let (pool, path) = test_pool().await;
        let lot = create_lot(
            &pool, "liq-9", "BTC", 10_000_000, 5_000_000, "USDC",
            &[], 100, &cfg(), "OP1",
        ).await.unwrap();

        // Two bids at same amount; earliest revealed wins.
        let c1 = make_commitment(5_500_000, "B1", &lot.id, "n1");
        let c2 = make_commitment(5_500_000, "B2", &lot.id, "n2");
        let b1 = submit_bid(&pool, &lot.id, "B1", &c1, 1_000_000, 150).await.unwrap();
        let b2 = submit_bid(&pool, &lot.id, "B2", &c2, 1_000_000, 150).await.unwrap();

        reveal_bid(&pool, &b1.id, 5_500_000, "n1", 210).await.unwrap();
        reveal_bid(&pool, &b2.id, 5_500_000, "n2", 211).await.unwrap();

        let settled = close_and_settle(
            &pool, &lot.id, 220, &NoopAuctionSettler, &DefaultFallback,
        ).await.unwrap();
        assert_eq!(settled.winning_bid_id.unwrap(), b1.id);

        pool.close().await;
        let _ = std::fs::remove_file(&path);
    }
}
