//! Backfill of historical blocks for the indexer.
//!
//! Blocks are streamed to the database in bounded batches so that memory use
//! stays flat regardless of how large the requested range is. Each batch is
//! written and committed before the next one is fetched, so a failure mid-way
//! leaves all previously written blocks intact and resume can continue from the
//! last persisted height.

use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::config::IndexerConfig;
use crate::store::BlockStore;
use crate::types::{Block, BlockHeight};

/// Default number of blocks written per batch when none is configured.
pub const DEFAULT_BACKFILL_BATCH_SIZE: usize = 500;

/// Streams blocks in `[from, to]` to the store in bounded batches.
///
/// Returns the height of the last block successfully written. On error the
/// caller can resume from the returned/known last written height without
/// losing already-persisted blocks.
pub async fn run_backfill(
    config: &IndexerConfig,
    store: Arc<dyn BlockStore>,
    from: BlockHeight,
    to: BlockHeight,
) -> Result<Option<BlockHeight>> {
    let batch_size = config.backfill_batch_size.max(1);
    info!(
        from = from.0,
        to = to.0,
        batch_size,
        "starting backfill in bounded batches"
    );

    let mut last_written: Option<BlockHeight> = None;
    let mut cursor = from;

    while cursor <= to {
        let batch_end = BlockHeight(cursor.0.saturating_add(batch_size as u64 - 1).min(to.0));

        // Fetch only this batch's blocks; nothing accumulates across iterations.
        let blocks = fetch_range(cursor, batch_end)
            .await
            .with_context(|| format!("fetching blocks {}..={}", cursor.0, batch_end.0))?;

        if blocks.is_empty() {
            debug!(from = cursor.0, to = batch_end.0, "empty batch, stopping");
            break;
        }

        // Write and commit this batch before moving on. A failure here leaves
        // earlier batches persisted, so resume continues from `last_written`.
        store
            .write_batch(&blocks)
            .await
            .with_context(|| format!("writing batch {}..={}", cursor.0, batch_end.0))?;

        let batch_last = blocks.last().map(|b| b.height).unwrap_or(batch_end);
        last_written = Some(batch_last);
        debug!(last = batch_last.0, "batch written");

        if batch_end >= to {
            break;
        }
        cursor = BlockHeight(batch_end.0.saturating_add(1));
    }

    if let Some(last) = last_written {
        info!(last = last.0, "backfill complete");
    } else {
        warn!("backfill produced no blocks");
    }

    Ok(last_written)
}

/// Fetches the blocks for an inclusive range. Kept separate so the batching
/// loop stays free of source-specific details.
async fn fetch_range(from: BlockHeight, to: BlockHeight) -> Result<Vec<Block>> {
    let mut blocks = Vec::with_capacity((to.0.saturating_sub(from.0) + 1) as usize);
    let mut height = from;
    while height <= to {
        blocks.push(fetch_block(height).await?);
        height = BlockHeight(height.0.saturating_add(1));
    }
    Ok(blocks)
}

/// Fetches a single block from the upstream source.
async fn fetch_block(height: BlockHeight) -> Result<Block> {
    crate::source::fetch_block(height)
        .await
        .with_context(|| format!("fetching block {}", height.0))
}
