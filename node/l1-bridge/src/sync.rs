use std::sync::Arc;

use kaspa_hashes::Hash as BlockHash;
use kaspa_rpc_core::{RpcBlock, api::rpc::RpcApi};
use kaspa_wrpc_client::KaspaRpcClient;

use crate::{ChainCoordinate, EventQueue, L1BridgeError, L1Event, Result, state::BridgeState};

/// Fetches a block by hash from the L1 node.
pub async fn fetch_block(client: &Arc<KaspaRpcClient>, hash: BlockHash) -> Result<RpcBlock> {
    client
        .get_block(hash, true)
        .await
        .map_err(|e| L1BridgeError::RpcCall(format!("get_block failed: {}", e)))
}

/// Gets the direct parents of a block (level 0 parents in the DAG).
pub async fn get_block_parents(
    client: &Arc<KaspaRpcClient>,
    hash: BlockHash,
) -> Result<Vec<BlockHash>> {
    let block = fetch_block(client, hash).await?;
    Ok(block.header.parents_by_level.first().cloned().unwrap_or_default())
}

/// Performs initial sync from last_processed to current virtual chain tip.
/// Emits BlockAdded events in order (past to present) with sequential indices.
/// Returns the last synced block coordinate.
pub async fn perform_initial_sync(
    client: &Arc<KaspaRpcClient>,
    last_processed: Option<ChainCoordinate>,
    state: &Arc<BridgeState>,
    event_queue: &EventQueue,
) -> Result<Option<ChainCoordinate>> {
    // Get DAG info for pruning point.
    let dag_info = client
        .get_block_dag_info()
        .await
        .map_err(|e| L1BridgeError::RpcCall(format!("get_block_dag_info failed: {}", e)))?;

    // Determine the starting point for sync.
    // If no last_processed is provided, start from the pruning point.
    let start_hash = last_processed.map(|c| c.hash()).unwrap_or(dag_info.pruning_point_hash);

    // If starting fresh (no last_processed), the pruning point is our starting point.
    // Record it for finalization tracking.
    if last_processed.is_none() {
        state
            .record_block(ChainCoordinate::new(dag_info.pruning_point_hash, state.initial_index()));
    }

    // Get current virtual chain state from the starting point.
    let virtual_chain = client
        .get_virtual_chain_from_block(
            start_hash, true, // include_accepted_transaction_ids
            None, // no page limit
        )
        .await
        .map_err(|e| {
            L1BridgeError::RpcCall(format!("get_virtual_chain_from_block failed: {}", e))
        })?;

    // If the starting block was reorged out, the RPC returns removed blocks.
    // This means our checkpoint is no longer in the main chain.
    if !virtual_chain.removed_chain_block_hashes.is_empty() {
        return Err(L1BridgeError::RpcCall(format!(
            "starting block {} was reorged out ({} blocks removed)",
            start_hash,
            virtual_chain.removed_chain_block_hashes.len()
        )));
    }

    let added_hashes = virtual_chain.added_chain_block_hashes;

    if added_hashes.is_empty() {
        log::info!("L1 bridge: already synced, no new blocks");
        return Ok(last_processed);
    }

    log::info!(
        "L1 bridge: syncing {} blocks from {:?}",
        added_hashes.len(),
        last_processed.map(|c| c.hash())
    );

    // Fetch and emit blocks in order with sequential indices.
    let mut last_block: Option<ChainCoordinate> = None;
    for &hash in added_hashes.iter() {
        let block = fetch_block(client, hash).await?;
        let index = state.next_index();
        let block_coord = ChainCoordinate::new(block.header.hash, index);
        last_block = Some(block_coord);
        // Record the hash->index mapping for finalization tracking.
        state.record_block(block_coord);
        event_queue.push(L1Event::BlockAdded { index, block: Box::new(block) });
    }

    Ok(last_block)
}
