use std::sync::Arc;

use kaspa_hashes::Hash as BlockHash;
use kaspa_rpc_core::{RpcBlock, api::rpc::RpcApi};
use kaspa_wrpc_client::KaspaRpcClient;

use crate::{EventQueue, L1BridgeError, L1Event, Result, state::BridgeState};

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
/// Also emits a Finalized event for the starting point.
/// Returns the hash of the last synced block.
pub async fn perform_initial_sync(
    client: &Arc<KaspaRpcClient>,
    last_processed: Option<BlockHash>,
    state: &Arc<BridgeState>,
    event_queue: &EventQueue,
) -> Result<Option<BlockHash>> {
    // Get DAG info for pruning point.
    let dag_info = client
        .get_block_dag_info()
        .await
        .map_err(|e| L1BridgeError::RpcCall(format!("get_block_dag_info failed: {}", e)))?;

    // Determine the starting point for sync.
    // If no last_processed is provided, start from the pruning point.
    let start_hash = last_processed.unwrap_or(dag_info.pruning_point_hash);

    // Emit Finalized for the starting point.
    // This tells the scheduler that blocks up to initial_index are finalized on L1.
    event_queue.push(L1Event::Finalized { index: state.initial_index(), hash: start_hash });

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

    let added_hashes = virtual_chain.added_chain_block_hashes;

    if added_hashes.is_empty() {
        log::info!("L1 bridge: already synced, no new blocks");
        return Ok(last_processed);
    }

    log::info!("L1 bridge: syncing {} blocks from {:?}", added_hashes.len(), last_processed);

    // Fetch and emit blocks in order with sequential indices.
    let mut last_hash = last_processed;
    for &hash in added_hashes.iter() {
        let block = fetch_block(client, hash).await?;
        let index = state.next_index();
        last_hash = Some(block.header.hash);
        // Record the hash->index mapping for finalization tracking.
        state.record_block(block.header.hash, index);
        event_queue.push(L1Event::BlockAdded { index, block: Box::new(block) });
    }

    Ok(last_hash)
}
