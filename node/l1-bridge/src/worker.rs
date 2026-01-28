use std::{sync::Arc, thread::JoinHandle, time::Duration};

use futures::{FutureExt, select_biased};
use kaspa_notify::scope::{PruningPointUtxoSetOverrideScope, Scope, VirtualChainChangedScope};
use kaspa_rpc_core::{
    Notification,
    api::{ctl::RpcState, rpc::RpcApi},
};
use kaspa_wrpc_client::prelude::*;
use tokio::{runtime::Builder, sync::Notify};
use workflow_core::channel::Channel;

use crate::{
    ChainCoordinate, EventQueue, L1BridgeConfig, L1Event,
    state::BridgeState,
    sync::{fetch_block, get_block_parents, perform_initial_sync},
};

/// Background worker that handles L1 node communication.
pub struct BridgeWorker {
    notify: Arc<Notify>,
    handle: Option<JoinHandle<()>>,
}

impl BridgeWorker {
    /// Spawns a new bridge worker.
    pub fn spawn(config: L1BridgeConfig, event_queue: EventQueue, state: Arc<BridgeState>) -> Self {
        let notify = Arc::new(Notify::new());
        let handle = Self::start(config, event_queue, state, notify.clone());
        Self { notify, handle: Some(handle) }
    }

    /// Signals the worker to shut down and waits for completion.
    pub fn shutdown(mut self) {
        self.notify.notify_one();
        if let Some(handle) = self.handle.take() {
            handle.join().expect("bridge worker panicked");
        }
    }

    fn start(
        config: L1BridgeConfig,
        event_queue: EventQueue,
        state: Arc<BridgeState>,
        notify: Arc<Notify>,
    ) -> JoinHandle<()> {
        std::thread::spawn(move || {
            Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime")
                .block_on(Self::run(config, event_queue, state, notify))
        })
    }

    async fn run(
        config: L1BridgeConfig,
        event_queue: EventQueue,
        state: Arc<BridgeState>,
        notify: Arc<Notify>,
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
                _ = notify.notified().fuse() => {
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

                            // Register notification listener and subscribe to notifications.
                            let id = client.rpc_api().register_new_listener(
                                ChannelConnection::new(
                                    "vprogs-l1-bridge",
                                    notification_channel.sender.clone(),
                                    ChannelType::Persistent,
                                )
                            );
                            for scope in [
                                Scope::VirtualChainChanged(VirtualChainChangedScope::new(true)),
                                Scope::PruningPointUtxoSetOverride(PruningPointUtxoSetOverrideScope {}),
                            ] {
                                if let Err(e) = client.rpc_api().start_notify(id, scope).await {
                                    log::error!("Failed to subscribe to notification: {}", e);
                                }
                            }
                            listener_id = Some(id);

                            // Emit connected event.
                            event_queue.push(L1Event::Connected);

                            // Perform initial sync if needed.
                            if needs_initial_sync {
                                let last_processed = state.last_processed();
                                match perform_initial_sync(&client, last_processed, &state, &event_queue).await {
                                    Ok(last_coord) => {
                                        if let Some(coord) = last_coord {
                                            state.set_last_processed(coord);
                                        }
                                        state.set_synced(true);
                                        event_queue.push(L1Event::Synced);
                                        needs_initial_sync = false;
                                        log::info!("L1 bridge initial sync complete");
                                    }
                                    Err(e) => {
                                        let error_msg = e.to_string().to_lowercase();
                                        // Check if error indicates starting block is pruned/reorged/not found.
                                        if error_msg.contains("not found")
                                            || error_msg.contains("pruned")
                                            || error_msg.contains("reorged")
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
                                            let rollback_coord =
                                                ChainCoordinate::new(common_ancestor, rollback_index);
                                            log::info!(
                                                "L1 bridge: rolling back to index {} (hash {})",
                                                rollback_index,
                                                common_ancestor
                                            );

                                            state.set_last_processed(rollback_coord);
                                            event_queue.push(L1Event::Rollback(rollback_coord));
                                        }
                                    }
                                }
                            }

                            // Process added blocks in order.
                            for &hash in vcc.added_chain_block_hashes.iter() {
                                match fetch_block(&client, hash).await {
                                    Ok(block) => {
                                        let index = state.next_index();
                                        let block_coord =
                                            ChainCoordinate::new(block.header.hash, index);
                                        state.set_last_processed(block_coord);
                                        // Record for finalization tracking.
                                        state.record_block(block_coord);
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
                                    // Look up the coordinate for this hash.
                                    if let Some(coord) = state.get_coordinate_for_hash(&pruning_hash) {
                                        // Only emit if this is a new finalization.
                                        let last_finalized_index =
                                            state.last_finalized().map(|c| c.index()).unwrap_or(0);
                                        if coord.index() > last_finalized_index {
                                            log::info!(
                                                "L1 bridge: pruning point advanced to index {} (hash {})",
                                                coord.index(),
                                                pruning_hash
                                            );
                                            state.set_last_finalized(coord);
                                            event_queue.push(L1Event::Finalized(coord));
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
}

impl Drop for BridgeWorker {
    fn drop(&mut self) {
        self.notify.notify_one();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
