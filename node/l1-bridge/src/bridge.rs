use std::{
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};

use futures::{FutureExt, select_biased};
use kaspa_hashes::Hash as BlockHash;
use kaspa_notify::scope::{PruningPointUtxoSetOverrideScope, Scope, VirtualChainChangedScope};
use kaspa_rpc_core::{
    Notification,
    api::{ctl::RpcState, rpc::RpcApi},
};
use kaspa_wrpc_client::prelude::*;
use tokio::{runtime::Builder, sync::Notify};
use workflow_core::channel::Channel;

use crate::{
    EventQueue, L1BridgeConfig, L1BridgeError, L1Event, Result,
    state::BridgeState,
    sync::{fetch_block, get_block_parents, perform_initial_sync},
};

/// The main L1 bridge that connects to Kaspa L1 and emits simplified events
/// suitable for scheduler integration.
pub struct L1Bridge {
    /// Configuration for the bridge.
    config: L1BridgeConfig,
    /// Lock-free event queue for consumers.
    event_queue: EventQueue,
    /// Bridge state tracking.
    state: Arc<BridgeState>,
    /// Notification signal to shut down the worker.
    notify_shutdown: Arc<Notify>,
    /// Handle to the background worker thread.
    handle: Option<JoinHandle<()>>,
}

impl L1Bridge {
    /// Creates a new L1 bridge with the given configuration.
    ///
    /// - `config`: Bridge configuration
    /// - `last_processed`: Hash of the last processed block (or None to start from pruning point)
    /// - `last_index`: Index of the last processed block (next block will be last_index + 1)
    ///
    /// The bridge does not connect automatically; call `start()` to initiate connection.
    pub fn new(config: L1BridgeConfig, last_processed: Option<BlockHash>, last_index: u64) -> Self {
        Self {
            config,
            event_queue: EventQueue::new(),
            state: Arc::new(BridgeState::new(last_processed, last_index)),
            notify_shutdown: Arc::new(Notify::new()),
            handle: None,
        }
    }

    /// Starts the bridge, initiating connection to the L1 node.
    ///
    /// This spawns a background worker thread that:
    /// 1. Connects to the L1 node
    /// 2. Performs initial sync from last_processed to current tip
    /// 3. Emits BlockAdded events in order with sequential indices
    /// 4. Emits Synced event when caught up
    /// 5. Switches to live streaming mode
    pub fn start(&mut self) -> Result<()> {
        if self.handle.is_some() {
            return Err(L1BridgeError::AlreadyStarted);
        }

        let config = self.config.clone();
        let event_queue = self.event_queue.clone();
        let state = self.state.clone();
        let notify_shutdown = self.notify_shutdown.clone();

        let handle = thread::spawn(move || {
            Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime")
                .block_on(run_event_loop(config, event_queue, state, notify_shutdown))
        });

        self.handle = Some(handle);
        Ok(())
    }

    /// Stops the bridge and disconnects from the L1 node.
    ///
    /// This signals the background worker to shut down and waits for it to complete.
    pub fn stop(&mut self) -> Result<()> {
        self.notify_shutdown.notify_one();
        if let Some(handle) = self.handle.take() {
            handle.join().expect("bridge worker panicked");
        }
        Ok(())
    }

    /// Returns the event queue for consuming L1 events.
    pub fn event_queue(&self) -> &EventQueue {
        &self.event_queue
    }

    /// Returns the bridge state.
    pub fn state(&self) -> &Arc<BridgeState> {
        &self.state
    }
}

impl Drop for L1Bridge {
    fn drop(&mut self) {
        // Signal shutdown and wait for the worker to complete.
        self.notify_shutdown.notify_one();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Runs the main event loop for the bridge worker.
async fn run_event_loop(
    config: L1BridgeConfig,
    event_queue: EventQueue,
    state: Arc<BridgeState>,
    notify_shutdown: Arc<Notify>,
) {
    // Create the RPC client.
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
            return;
        }
    };

    // Create notification channel for receiving node notifications.
    let notification_channel: Channel<Notification> = Channel::unbounded();

    // Get RPC control channel for connection events.
    let rpc_ctl_channel = client.rpc_ctl().multiplexer().channel();

    // Connect with configured options.
    let connect_options = ConnectOptions {
        block_async_connect: config.block_async_connect,
        connect_timeout: Some(Duration::from_millis(config.connect_timeout_ms)),
        strategy: config.connect_strategy,
        ..Default::default()
    };

    if let Err(e) = client.connect(Some(connect_options)).await {
        log::error!("Failed to initiate connection: {}", e);
        return;
    }

    let mut listener_id: Option<ListenerId> = None;
    let mut needs_initial_sync = true;

    loop {
        select_biased! {
            // Handle shutdown signal (lowest priority to drain other channels first).
            _ = notify_shutdown.notified().fuse() => {
                log::info!("L1 bridge shutdown requested");
                break;
            }

            // Handle RPC connection state changes.
            msg = rpc_ctl_channel.receiver.recv().fuse() => {
                match msg {
                    Ok(RpcState::Connected) => {
                        log::info!("L1 bridge connected to {}", client.url().unwrap_or_default());
                        state.set_connected(true);
                        state.increment_reconnect_count();

                        // Register notification listener.
                        let id = client.rpc_api().register_new_listener(
                            ChannelConnection::new(
                                "vprogs-l1-bridge",
                                notification_channel.sender.clone(),
                                ChannelType::Persistent,
                            )
                        );

                        // Subscribe to VirtualChainChanged for reorg detection.
                        let vcc_scope = Scope::VirtualChainChanged(VirtualChainChangedScope {
                            include_accepted_transaction_ids: true,
                        });

                        if let Err(e) = client.rpc_api().start_notify(id, vcc_scope).await {
                            log::error!("Failed to subscribe to VirtualChainChanged: {}", e);
                        }

                        // Subscribe to PruningPointUtxoSetOverride for finalization tracking.
                        let pruning_scope =
                            Scope::PruningPointUtxoSetOverride(PruningPointUtxoSetOverrideScope {});

                        if let Err(e) = client.rpc_api().start_notify(id, pruning_scope).await {
                            log::error!("Failed to subscribe to PruningPointUtxoSetOverride: {}", e);
                        }

                        listener_id = Some(id);

                        // Emit connected event.
                        event_queue.push(L1Event::Connected);

                        // Perform initial sync if needed.
                        if needs_initial_sync {
                            let last_processed = state.last_block_hash();
                            match perform_initial_sync(&client, last_processed, &state, &event_queue).await {
                                Ok(last_hash) => {
                                    if let Some(hash) = last_hash {
                                        state.set_last_block_hash(hash);
                                    }
                                    state.set_synced(true);
                                    event_queue.push(L1Event::Synced);
                                    needs_initial_sync = false;
                                    log::info!("L1 bridge initial sync complete");
                                }
                                Err(e) => {
                                    let error_msg = e.to_string().to_lowercase();
                                    // Check if error indicates starting block is pruned/not found.
                                    if error_msg.contains("not found")
                                        || error_msg.contains("pruned")
                                        || error_msg.contains("not in chain")
                                        || error_msg.contains("block is not in")
                                    {
                                        log::error!(
                                            "L1 bridge: starting block no longer in chain: {}",
                                            e
                                        );
                                        event_queue.push(L1Event::SyncLost {
                                            reason: format!(
                                                "Starting block no longer in chain (pruned or reorged): {}",
                                                e
                                            ),
                                        });
                                        // Don't retry - consumer must restart with valid checkpoint.
                                        needs_initial_sync = false;
                                    } else {
                                        log::error!("Initial sync failed: {}", e);
                                        // Will retry on next connection for transient errors.
                                    }
                                }
                            }
                        }
                    }
                    Ok(RpcState::Disconnected) => {
                        log::info!("L1 bridge disconnected");
                        state.set_connected(false);

                        // Unregister listener.
                        if let Some(id) = listener_id.take() {
                            let _ = client.rpc_api().unregister_listener(id).await;
                        }

                        // Emit disconnected event.
                        event_queue.push(L1Event::Disconnected);
                    }
                    Err(e) => {
                        log::error!("RPC control channel error: {}", e);
                        break;
                    }
                }
            }

            // Handle notifications from the node (live mode).
            notification = notification_channel.receiver.recv().fuse() => {
                match notification {
                    Ok(Notification::VirtualChainChanged(vcc)) => {
                        // Handle reorg if blocks were removed.
                        if !vcc.removed_chain_block_hashes.is_empty() {
                            log::info!(
                                "L1 bridge: reorg detected, {} blocks removed",
                                vcc.removed_chain_block_hashes.len()
                            );

                            // Calculate the rollback index.
                            let num_removed = vcc.removed_chain_block_hashes.len() as u64;
                            let current = state.current_index();
                            let rollback_index = current.saturating_sub(num_removed);

                            if let Some(&first_added) = vcc.added_chain_block_hashes.first() {
                                // Get the parents of the first added block to find the common ancestor.
                                if let Ok(parents) = get_block_parents(&client, first_added).await {
                                    if let Some(&common_ancestor) = parents.first() {
                                        log::info!(
                                            "L1 bridge: rolling back to index {} (hash {})",
                                            rollback_index,
                                            common_ancestor
                                        );

                                        state.set_index(rollback_index);
                                        state.set_last_block_hash(common_ancestor);
                                        event_queue.push(L1Event::Rollback {
                                            to_index: rollback_index,
                                            to_hash: common_ancestor,
                                        });
                                    }
                                }
                            }
                        }

                        // Process added blocks in order.
                        for &hash in vcc.added_chain_block_hashes.iter() {
                            match fetch_block(&client, hash).await {
                                Ok(block) => {
                                    let index = state.next_index();
                                    state.set_last_block_hash(block.header.hash);
                                    // Record for finalization tracking.
                                    state.record_block(block.header.hash, index);
                                    event_queue.push(L1Event::BlockAdded {
                                        index,
                                        block: Box::new(block),
                                    });
                                }
                                Err(e) => {
                                    log::error!("Failed to fetch block {}: {}", hash, e);
                                }
                            }
                        }
                    }
                    Ok(Notification::PruningPointUtxoSetOverride(_)) => {
                        // Pruning point has advanced - query current pruning point.
                        match client.get_block_dag_info().await {
                            Ok(dag_info) => {
                                let pruning_hash = dag_info.pruning_point_hash;
                                // Look up the index for this hash.
                                if let Some(index) = state.get_index_for_hash(&pruning_hash) {
                                    // Only emit if this is a new finalization.
                                    if index > state.last_finalized_index() {
                                        log::info!(
                                            "L1 bridge: pruning point advanced to index {} (hash {})",
                                            index,
                                            pruning_hash
                                        );
                                        state.set_finalized(index);
                                        event_queue.push(L1Event::Finalized {
                                            index,
                                            hash: pruning_hash,
                                        });
                                    }
                                } else {
                                    log::debug!(
                                        "L1 bridge: pruning point {} not in tracked blocks",
                                        pruning_hash
                                    );
                                }
                            }
                            Err(e) => {
                                log::error!("Failed to get block dag info: {}", e);
                            }
                        }
                    }
                    Ok(_) => {
                        // Ignore other notification types.
                    }
                    Err(e) => {
                        log::error!("Notification channel error: {}", e);
                        break;
                    }
                }
            }
        }
    }

    // Cleanup: unregister listener and disconnect.
    if let Some(id) = listener_id {
        let _ = client.rpc_api().unregister_listener(id).await;
    }
    let _ = client.disconnect().await;
    state.set_connected(false);
    log::info!("L1 bridge worker stopped");
}
