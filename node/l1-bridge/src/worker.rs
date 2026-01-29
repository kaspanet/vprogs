use std::{sync::Arc, time::Duration};

use crossbeam_queue::SegQueue;
use futures::{FutureExt, select_biased};
use kaspa_hashes::Hash as BlockHash;
use kaspa_notify::scope::{PruningPointUtxoSetOverrideScope, Scope, VirtualChainChangedScope};
use kaspa_rpc_core::{
    Notification, VirtualChainChangedNotification,
    api::{ctl::RpcState, rpc::RpcApi},
};
use kaspa_wrpc_client::prelude::*;
use tokio::sync::Notify;
use workflow_core::channel::Channel;

use crate::{
    ChainCoordinate, L1BridgeConfig, L1BridgeError, L1Event, Result, chain_state::ChainState,
    kaspa_rpc_client_ext::KaspaRpcClientExt,
};

/// Background worker for L1 communication.
pub struct BridgeWorker {
    /// RPC client for L1 node communication.
    client: Arc<KaspaRpcClient>,
    /// Tracks processed and finalized blocks.
    chain_state: ChainState,
    /// Shared queue for emitting events to consumers.
    queue: Arc<SegQueue<L1Event>>,
    /// Signal to wake consumers waiting for events.
    event_signal: Arc<Notify>,
    /// Channel for receiving L1 chain notifications.
    notification_channel: Channel<Notification>,
    /// Channel for RPC connection state changes.
    rpc_ctl_channel: workflow_core::channel::MultiplexerChannel<RpcState>,
    /// Active notification listener ID, if subscribed.
    listener_id: Option<ListenerId>,
    /// Whether initial sync is still pending.
    needs_initial_sync: bool,
}

impl BridgeWorker {
    /// Creates a new bridge worker.
    pub async fn new(
        config: &L1BridgeConfig,
        queue: Arc<SegQueue<L1Event>>,
        event_signal: Arc<Notify>,
    ) -> Option<Self> {
        let chain_state = ChainState::new(config.last_processed, config.last_finalized);

        // Use resolver for public node discovery when no explicit URL is provided.
        let resolver = if config.url.is_none() { Some(Resolver::default()) } else { None };
        let client = match KaspaRpcClient::new_with_args(
            WrpcEncoding::Borsh,
            config.url.as_deref(),
            resolver,
            Some(config.network_id),
            None,
        ) {
            Ok(client) => Arc::new(client),
            Err(e) => {
                log::error!("Failed to create RPC client: {}", e);
                return None;
            }
        };

        // Get RPC control channel BEFORE connecting to not miss the Connected event.
        let rpc_ctl_channel = client.rpc_ctl().multiplexer().channel();

        // Initiate connection.
        if let Err(e) = client
            .connect(Some(ConnectOptions {
                block_async_connect: true,
                connect_timeout: Some(Duration::from_millis(config.connect_timeout_ms)),
                strategy: config.connect_strategy,
                ..Default::default()
            }))
            .await
        {
            log::error!("Failed to initiate connection: {}", e);
            return None;
        }

        Some(Self {
            client,
            chain_state,
            queue,
            event_signal,
            notification_channel: Channel::unbounded(),
            rpc_ctl_channel,
            listener_id: None,
            needs_initial_sync: true,
        })
    }

    /// Runs the event loop.
    pub async fn run(mut self, shutdown: Arc<Notify>) {
        loop {
            // Priority: shutdown > connection state > chain notifications.
            select_biased! {
                _ = shutdown.notified().fuse() => {
                    log::info!("L1 bridge shutdown requested");
                    break;
                }

                msg = self.rpc_ctl_channel.receiver.recv().fuse() => {
                    match msg {
                        Ok(RpcState::Connected) => self.handle_connected().await,
                        Ok(RpcState::Disconnected) => self.handle_disconnected().await,
                        Err(e) => {
                            log::error!("RPC control channel error: {}", e);
                            break;
                        }
                    }
                }

                notification = self.notification_channel.receiver.recv().fuse() => {
                    match notification {
                        Ok(Notification::VirtualChainChanged(vcc)) => {
                            self.handle_chain_changed(vcc).await;
                        }
                        Ok(Notification::PruningPointUtxoSetOverride(_)) => {
                            self.handle_finalization().await;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            log::error!("Notification channel error: {}", e);
                            break;
                        }
                    }
                }
            }
        }

        self.cleanup().await;
        log::info!("L1 bridge worker stopped");
    }

    /// Emits an event.
    fn push_event(&self, event: L1Event) {
        self.queue.push(event);
        self.event_signal.notify_one();
    }

    /// Cleans up resources.
    async fn cleanup(&mut self) {
        if let Some(id) = self.listener_id.take() {
            let _ = self.client.rpc_api().unregister_listener(id).await;
        }
        let _ = self.client.disconnect().await;
    }
}

// ============================================================================
// Connection Handlers
// ============================================================================

impl BridgeWorker {
    /// Handles connection.
    async fn handle_connected(&mut self) {
        log::info!("L1 bridge connected to {}", self.client.url().unwrap_or_default());

        self.subscribe_to_notifications().await;
        self.push_event(L1Event::Connected);

        if self.needs_initial_sync {
            self.perform_initial_sync().await;
        }
    }

    /// Handles disconnection.
    async fn handle_disconnected(&mut self) {
        log::info!("L1 bridge disconnected");

        if let Some(id) = self.listener_id.take() {
            let _ = self.client.rpc_api().unregister_listener(id).await;
        }

        self.push_event(L1Event::Disconnected);
    }

    /// Subscribes to chain notifications.
    async fn subscribe_to_notifications(&mut self) {
        let id = self.client.rpc_api().register_new_listener(ChannelConnection::new(
            "vprogs-l1-bridge",
            self.notification_channel.sender.clone(),
            ChannelType::Persistent,
        ));

        // Subscribe to chain changes (new blocks, reorgs) and finalization (pruning point).
        for scope in [
            Scope::VirtualChainChanged(VirtualChainChangedScope::new(true)),
            Scope::PruningPointUtxoSetOverride(PruningPointUtxoSetOverrideScope {}),
        ] {
            if let Err(e) = self.client.rpc_api().start_notify(id, scope).await {
                log::error!("Failed to subscribe to notification: {}", e);
            }
        }

        self.listener_id = Some(id);
    }
}

// ============================================================================
// Sync Handlers
// ============================================================================

impl BridgeWorker {
    /// Performs initial sync.
    async fn perform_initial_sync(&mut self) {
        let last_processed = self.chain_state.last_processed();

        match self.sync_from_checkpoint(last_processed).await {
            Ok(last_coord) => {
                if let Some(coord) = last_coord {
                    self.chain_state.set_last_processed(coord);
                }
                self.push_event(L1Event::Synced);
                self.needs_initial_sync = false;
                log::info!("L1 bridge initial sync complete");
            }
            Err(e) => self.handle_sync_error(e),
        }
    }

    /// Syncs blocks from a checkpoint.
    async fn sync_from_checkpoint(
        &mut self,
        last_processed: Option<ChainCoordinate>,
    ) -> Result<Option<ChainCoordinate>> {
        let dag_info = self
            .client
            .get_block_dag_info()
            .await
            .map_err(|e| L1BridgeError::RpcCall(format!("get_block_dag_info failed: {}", e)))?;

        let start_hash = last_processed.map(|c| c.hash()).unwrap_or(dag_info.pruning_point_hash);

        // Record pruning point if starting fresh.
        if last_processed.is_none() {
            self.chain_state.record_block(ChainCoordinate::new(
                dag_info.pruning_point_hash,
                self.chain_state.initial_index(),
            ));
        }

        let virtual_chain =
            self.client.get_virtual_chain_from_block(start_hash, true, None).await.map_err(
                |e| L1BridgeError::RpcCall(format!("get_virtual_chain_from_block failed: {}", e)),
            )?;

        // Check if our checkpoint was reorged out.
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

        let mut last_block = None;
        for &hash in &*added_hashes {
            if let Some(coord) = self.fetch_and_emit_block(hash).await {
                last_block = Some(coord);
            }
        }

        Ok(last_block)
    }

    /// Handles sync errors.
    fn handle_sync_error(&mut self, e: L1BridgeError) {
        let error_msg = e.to_string().to_lowercase();

        let is_checkpoint_lost = error_msg.contains("not found")
            || error_msg.contains("pruned")
            || error_msg.contains("reorged")
            || error_msg.contains("not in chain")
            || error_msg.contains("block is not in");

        if is_checkpoint_lost {
            log::error!("L1 bridge: starting block no longer in chain: {}", e);
            self.push_event(L1Event::SyncLost {
                reason: format!("Starting block no longer in chain (pruned or reorged): {}", e),
            });
            self.needs_initial_sync = false;
        } else {
            log::error!("Initial sync failed: {}", e);
            // Will retry on next connection.
        }
    }
}

// ============================================================================
// Chain Event Handlers
// ============================================================================

impl BridgeWorker {
    /// Handles a chain change.
    async fn handle_chain_changed(&mut self, vcc: VirtualChainChangedNotification) {
        if !vcc.removed_chain_block_hashes.is_empty() {
            self.handle_reorg(&vcc).await;
        }

        for &hash in vcc.added_chain_block_hashes.iter() {
            self.fetch_and_emit_block(hash).await;
        }
    }

    /// Handles a reorg.
    async fn handle_reorg(&mut self, vcc: &VirtualChainChangedNotification) {
        log::info!(
            "L1 bridge: reorg detected, {} blocks removed",
            vcc.removed_chain_block_hashes.len()
        );

        // Calculate the index we're rolling back to.
        let num_removed = vcc.removed_chain_block_hashes.len() as u64;
        let rollback_index = self.chain_state.current_index().saturating_sub(num_removed);

        // Find the common ancestor: parent of the first block in the new chain.
        let Some(&first_added) = vcc.added_chain_block_hashes.first() else {
            return;
        };

        let Ok(parents) = self.client.fetch_block_parents(first_added).await else {
            return;
        };

        let Some(&common_ancestor) = parents.first() else {
            return;
        };

        let rollback_coord = ChainCoordinate::new(common_ancestor, rollback_index);
        log::info!(
            "L1 bridge: rolling back to index {} (hash {})",
            rollback_index,
            common_ancestor
        );

        self.chain_state.set_last_processed(rollback_coord);
        self.push_event(L1Event::Rollback(rollback_coord));
    }

    /// Handles finalization.
    async fn handle_finalization(&mut self) {
        // Fetch current pruning point from the node.
        let dag_info = match self.client.get_block_dag_info().await {
            Ok(info) => info,
            Err(e) => {
                log::error!("Failed to get block dag info: {}", e);
                return;
            }
        };

        let pruning_hash = dag_info.pruning_point_hash;

        // Look up the index we assigned to this block when we processed it.
        let Some(coord) = self.chain_state.get_coordinate_for_hash(&pruning_hash) else {
            log::debug!("L1 bridge: pruning point {} not in tracked blocks", pruning_hash);
            return;
        };

        // Only emit if the pruning point actually advanced.
        let last_finalized_index =
            self.chain_state.last_finalized().map(|c| c.index()).unwrap_or(0);

        if coord.index() > last_finalized_index {
            log::info!(
                "L1 bridge: pruning point advanced to index {} (hash {})",
                coord.index(),
                pruning_hash
            );
            self.chain_state.set_last_finalized(coord);
            self.push_event(L1Event::Finalized(coord));
        }
    }

    /// Fetches and emits a block.
    async fn fetch_and_emit_block(&mut self, hash: BlockHash) -> Option<ChainCoordinate> {
        let block = match self.client.fetch_block(hash).await {
            Ok(block) => block,
            Err(e) => {
                log::error!("Failed to fetch block {}: {}", hash, e);
                return None;
            }
        };

        // Assign sequential index and update state.
        let index = self.chain_state.next_index();
        let coord = ChainCoordinate::new(block.header.hash, index);

        self.chain_state.set_last_processed(coord);
        self.chain_state.record_block(coord);
        self.push_event(L1Event::BlockAdded { index, block: Box::new(block) });

        Some(coord)
    }
}
