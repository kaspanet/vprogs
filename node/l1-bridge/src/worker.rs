use std::{sync::Arc, thread::JoinHandle, time::Duration};

use crossbeam_queue::SegQueue;
use futures::{FutureExt, select_biased};
use kaspa_hashes::Hash as BlockHash;
use kaspa_notify::scope::{PruningPointUtxoSetOverrideScope, Scope, VirtualChainChangedScope};
use kaspa_rpc_core::{
    Notification, RpcBlock, VirtualChainChangedNotification,
    api::{ctl::RpcState, rpc::RpcApi},
};
use kaspa_wrpc_client::prelude::*;
use tokio::{runtime::Builder, sync::Notify};
use workflow_core::channel::Channel;

use crate::{ChainCoordinate, L1BridgeConfig, L1BridgeError, L1Event, Result, state::BridgeState};

/// Background worker that handles L1 node communication.
pub struct BridgeWorker {
    shutdown: Arc<Notify>,
    handle: Option<JoinHandle<()>>,
}

impl BridgeWorker {
    /// Spawns a new bridge worker.
    pub fn spawn(
        config: L1BridgeConfig,
        queue: Arc<SegQueue<L1Event>>,
        event_signal: Arc<Notify>,
    ) -> Self {
        let shutdown = Arc::new(Notify::new());
        let handle = Self::start(config, queue, event_signal, shutdown.clone());
        Self { shutdown, handle: Some(handle) }
    }

    /// Signals the worker to shut down and waits for completion.
    pub fn shutdown(mut self) {
        self.shutdown.notify_one();
        if let Some(handle) = self.handle.take() {
            handle.join().expect("bridge worker panicked");
        }
    }

    fn start(
        config: L1BridgeConfig,
        queue: Arc<SegQueue<L1Event>>,
        event_signal: Arc<Notify>,
        shutdown: Arc<Notify>,
    ) -> JoinHandle<()> {
        std::thread::spawn(move || {
            Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime")
                .block_on(run_event_loop(config, queue, event_signal, shutdown))
        })
    }
}

impl Drop for BridgeWorker {
    fn drop(&mut self) {
        self.shutdown.notify_one();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

// ============================================================================
// Event Loop
// ============================================================================

async fn run_event_loop(
    config: L1BridgeConfig,
    queue: Arc<SegQueue<L1Event>>,
    event_signal: Arc<Notify>,
    shutdown: Arc<Notify>,
) {
    let Some(mut ctx) = WorkerContext::new(config, queue, event_signal).await else {
        return;
    };

    loop {
        select_biased! {
            _ = shutdown.notified().fuse() => {
                log::info!("L1 bridge shutdown requested");
                break;
            }

            msg = ctx.rpc_ctl_channel.receiver.recv().fuse() => {
                match msg {
                    Ok(RpcState::Connected) => ctx.handle_connected().await,
                    Ok(RpcState::Disconnected) => ctx.handle_disconnected().await,
                    Err(e) => {
                        log::error!("RPC control channel error: {}", e);
                        break;
                    }
                }
            }

            notification = ctx.notification_channel.receiver.recv().fuse() => {
                match notification {
                    Ok(Notification::VirtualChainChanged(vcc)) => {
                        ctx.handle_chain_changed(vcc).await;
                    }
                    Ok(Notification::PruningPointUtxoSetOverride(_)) => {
                        ctx.handle_finalization().await;
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

    ctx.cleanup().await;
    log::info!("L1 bridge worker stopped");
}

// ============================================================================
// Worker Context
// ============================================================================

/// Holds the runtime state for the worker event loop.
struct WorkerContext {
    client: Arc<KaspaRpcClient>,
    state: BridgeState,
    queue: Arc<SegQueue<L1Event>>,
    event_signal: Arc<Notify>,
    notification_channel: Channel<Notification>,
    rpc_ctl_channel: workflow_core::channel::MultiplexerChannel<RpcState>,
    listener_id: Option<ListenerId>,
    needs_initial_sync: bool,
}

impl WorkerContext {
    /// Creates a new worker context, connecting to the L1 node.
    async fn new(
        config: L1BridgeConfig,
        queue: Arc<SegQueue<L1Event>>,
        event_signal: Arc<Notify>,
    ) -> Option<Self> {
        let state = BridgeState::new(config.last_processed, config.last_finalized);

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

        let connect_options = ConnectOptions {
            block_async_connect: true,
            connect_timeout: Some(Duration::from_millis(config.connect_timeout_ms)),
            strategy: config.connect_strategy,
            ..Default::default()
        };

        if let Err(e) = client.connect(Some(connect_options)).await {
            log::error!("Failed to initiate connection: {}", e);
            return None;
        }

        Some(Self {
            client,
            state,
            queue,
            event_signal,
            notification_channel: Channel::unbounded(),
            rpc_ctl_channel,
            listener_id: None,
            needs_initial_sync: true,
        })
    }

    /// Emits an event to the queue and signals consumers.
    fn push_event(&self, event: L1Event) {
        self.queue.push(event);
        self.event_signal.notify_one();
    }

    /// Cleans up resources on shutdown.
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

impl WorkerContext {
    /// Handles successful connection to the L1 node.
    async fn handle_connected(&mut self) {
        log::info!("L1 bridge connected to {}", self.client.url().unwrap_or_default());

        self.subscribe_to_notifications().await;
        self.push_event(L1Event::Connected);

        if self.needs_initial_sync {
            self.perform_initial_sync().await;
        }
    }

    /// Handles disconnection from the L1 node.
    async fn handle_disconnected(&mut self) {
        log::info!("L1 bridge disconnected");

        if let Some(id) = self.listener_id.take() {
            let _ = self.client.rpc_api().unregister_listener(id).await;
        }

        self.push_event(L1Event::Disconnected);
    }

    /// Subscribes to chain notifications from the L1 node.
    async fn subscribe_to_notifications(&mut self) {
        let id = self.client.rpc_api().register_new_listener(ChannelConnection::new(
            "vprogs-l1-bridge",
            self.notification_channel.sender.clone(),
            ChannelType::Persistent,
        ));

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

impl WorkerContext {
    /// Performs initial sync from last checkpoint to current chain tip.
    async fn perform_initial_sync(&mut self) {
        let last_processed = self.state.last_processed();

        match self.sync_from_checkpoint(last_processed).await {
            Ok(last_coord) => {
                if let Some(coord) = last_coord {
                    self.state.set_last_processed(coord);
                }
                self.push_event(L1Event::Synced);
                self.needs_initial_sync = false;
                log::info!("L1 bridge initial sync complete");
            }
            Err(e) => self.handle_sync_error(e),
        }
    }

    /// Syncs blocks from a checkpoint to the current chain tip.
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
            self.state.record_block(ChainCoordinate::new(
                dag_info.pruning_point_hash,
                self.state.initial_index(),
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
        for &hash in &added_hashes {
            if let Some(coord) = self.fetch_and_emit_block(hash).await {
                last_block = Some(coord);
            }
        }

        Ok(last_block)
    }

    /// Handles sync errors, distinguishing recoverable from fatal errors.
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

impl WorkerContext {
    /// Handles a virtual chain change notification.
    async fn handle_chain_changed(&mut self, vcc: VirtualChainChangedNotification) {
        if !vcc.removed_chain_block_hashes.is_empty() {
            self.handle_reorg(&vcc).await;
        }

        for &hash in vcc.added_chain_block_hashes.iter() {
            self.fetch_and_emit_block(hash).await;
        }
    }

    /// Handles a chain reorganization.
    async fn handle_reorg(&mut self, vcc: &VirtualChainChangedNotification) {
        log::info!(
            "L1 bridge: reorg detected, {} blocks removed",
            vcc.removed_chain_block_hashes.len()
        );

        let num_removed = vcc.removed_chain_block_hashes.len() as u64;
        let rollback_index = self.state.current_index().saturating_sub(num_removed);

        let Some(&first_added) = vcc.added_chain_block_hashes.first() else {
            return;
        };

        let Ok(parents) = fetch_block_parents(&self.client, first_added).await else {
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

        self.state.set_last_processed(rollback_coord);
        self.push_event(L1Event::Rollback(rollback_coord));
    }

    /// Handles a pruning point advancement (finalization).
    async fn handle_finalization(&mut self) {
        let dag_info = match self.client.get_block_dag_info().await {
            Ok(info) => info,
            Err(e) => {
                log::error!("Failed to get block dag info: {}", e);
                return;
            }
        };

        let pruning_hash = dag_info.pruning_point_hash;

        let Some(coord) = self.state.get_coordinate_for_hash(&pruning_hash) else {
            log::debug!("L1 bridge: pruning point {} not in tracked blocks", pruning_hash);
            return;
        };

        let last_finalized_index = self.state.last_finalized().map(|c| c.index()).unwrap_or(0);

        if coord.index() > last_finalized_index {
            log::info!(
                "L1 bridge: pruning point advanced to index {} (hash {})",
                coord.index(),
                pruning_hash
            );
            self.state.set_last_finalized(coord);
            self.push_event(L1Event::Finalized(coord));
        }
    }

    /// Fetches a block and emits a BlockAdded event.
    async fn fetch_and_emit_block(&mut self, hash: BlockHash) -> Option<ChainCoordinate> {
        let block = match fetch_block(&self.client, hash).await {
            Ok(block) => block,
            Err(e) => {
                log::error!("Failed to fetch block {}: {}", hash, e);
                return None;
            }
        };

        let index = self.state.next_index();
        let coord = ChainCoordinate::new(block.header.hash, index);

        self.state.set_last_processed(coord);
        self.state.record_block(coord);
        self.push_event(L1Event::BlockAdded { index, block: Box::new(block) });

        Some(coord)
    }
}

// ============================================================================
// RPC Helpers
// ============================================================================

async fn fetch_block(client: &KaspaRpcClient, hash: BlockHash) -> Result<RpcBlock> {
    client
        .get_block(hash, true)
        .await
        .map_err(|e| L1BridgeError::RpcCall(format!("get_block failed: {}", e)))
}

async fn fetch_block_parents(client: &KaspaRpcClient, hash: BlockHash) -> Result<Vec<BlockHash>> {
    let block = fetch_block(client, hash).await?;
    Ok(block.header.parents_by_level.first().cloned().unwrap_or_default())
}
