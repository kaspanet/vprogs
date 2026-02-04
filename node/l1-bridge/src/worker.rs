use std::{sync::Arc, time::Duration};

use crossbeam_queue::SegQueue;
use futures::{FutureExt, select_biased};
use kaspa_notify::scope::{PruningPointUtxoSetOverrideScope, Scope, VirtualChainChangedScope};
use kaspa_rpc_core::{
    GetVirtualChainFromBlockV2Response, Notification, RpcDataVerbosityLevel,
    api::{ctl::RpcState, rpc::RpcApi},
};
use kaspa_wrpc_client::prelude::*;
use tokio::sync::Notify;
use workflow_core::channel::Channel;

use crate::{
    ChainBlock, L1BridgeConfig, L1Event,
    error::{Error, Result},
    virtual_chain::VirtualChain,
};

/// Background worker for L1 communication.
pub struct BridgeWorker {
    /// RPC client for L1 node communication.
    client: Arc<KaspaRpcClient>,
    /// Tracks processed blocks and finalization.
    virtual_chain: VirtualChain,
    /// Shared queue for emitting events to consumers.
    queue: Arc<SegQueue<L1Event>>,
    /// Signal to wake consumers waiting for events.
    event_signal: Arc<Notify>,
    /// Channel for receiving L1 chain notifications.
    notification_channel: Channel<Notification>,
    /// Channel for RPC connection state changes.
    rpc_ctl_channel: workflow_core::channel::MultiplexerChannel<RpcState>,
    /// Whether a fatal error has occurred.
    fatal: bool,
    /// Target coordinate for gap recovery when resuming with a separate root.
    /// Once the gap is filled on first connect, this is set to None.
    recovery_target: Option<ChainBlock>,
}

impl BridgeWorker {
    /// Creates a new bridge worker.
    pub async fn new(
        config: &L1BridgeConfig,
        queue: Arc<SegQueue<L1Event>>,
        event_signal: Arc<Notify>,
    ) -> Option<Self> {
        // Initialize virtual chain: prefer root, fall back to tip, or use
        // a sentinel root. When starting fresh (sentinel root), fetch_chain_updates will
        // query the L1 pruning point to begin syncing.
        let root = config.root.clone().or(config.tip.clone()).unwrap_or_default();
        let virtual_chain = VirtualChain::new(root);

        // When root and tip are both set and differ, we need to recover the
        // gap between them on first connect (lightweight sync without verbose block data).
        let recovery_target = match (&config.root, &config.tip) {
            (Some(root), Some(tip)) if root.hash() != tip.hash() => Some(tip.clone()),
            _ => None,
        };

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
                let reason = format!("failed to create RPC client: {}", e);
                log::error!("L1 bridge: {}", reason);
                queue.push(L1Event::Fatal { reason });
                event_signal.notify_one();
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
            let reason = format!("failed to connect: {}", e);
            log::error!("L1 bridge: {}", reason);
            queue.push(L1Event::Fatal { reason });
            event_signal.notify_one();
            return None;
        }

        Some(Self {
            client,
            virtual_chain,
            queue,
            event_signal,
            notification_channel: Channel::unbounded(),
            rpc_ctl_channel,
            fatal: false,
            recovery_target,
        })
    }

    /// Runs the event loop.
    pub async fn run(mut self, shutdown: Arc<Notify>) {
        loop {
            if self.fatal {
                log::error!("L1 bridge: stopping due to fatal error");
                break;
            }

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
                            self.fatal_error(format!("RPC control channel closed: {}", e));
                        }
                    }
                }

                notification = self.notification_channel.receiver.recv().fuse() => {
                    match notification {
                        Ok(Notification::VirtualChainChanged(_)) => {
                            let result = self.fetch_chain_updates().await;
                            self.handle_sync_result(result);
                        }
                        Ok(Notification::PruningPointUtxoSetOverride(_)) => {
                            let result = self.handle_finalization().await;
                            self.handle_sync_result(result);
                        }
                        Ok(_) => {}
                        Err(e) => {
                            self.fatal_error(format!("notification channel closed: {}", e));
                        }
                    }
                }
            }
        }

        let _ = self.client.disconnect().await;
        log::info!("L1 bridge worker stopped");
    }

    /// Emits an event.
    fn push_event(&self, event: L1Event) {
        self.queue.push(event);
        self.event_signal.notify_one();
    }

    /// Emits a fatal error event and marks the worker for shutdown.
    fn fatal_error(&mut self, reason: String) {
        log::error!("L1 bridge fatal error: {}", reason);
        self.push_event(L1Event::Fatal { reason });
        self.fatal = true;
    }
}

// ============================================================================
// Connection Handlers
// ============================================================================

impl BridgeWorker {
    /// Handles connection.
    async fn handle_connected(&mut self) {
        log::info!("L1 bridge connected to {}", self.client.url().unwrap_or_default());

        if let Err(e) = self.subscribe_to_notifications().await {
            self.fatal_error(format!("failed to subscribe to notifications: {}", e));
            return;
        }

        // Recover the gap between root and tip if needed.
        if let Some(target) = self.recovery_target.take() {
            let result = self.recover_gap(&target).await;
            self.handle_sync_result(result);
            if self.fatal {
                return;
            }
        }

        self.push_event(L1Event::Connected);

        // Sync up to current chain state.
        let result = self.fetch_chain_updates().await;
        self.handle_sync_result(result);
    }

    /// Handles disconnection.
    async fn handle_disconnected(&mut self) {
        log::info!("L1 bridge disconnected");
        self.push_event(L1Event::Disconnected);
    }

    /// Subscribes to chain notifications.
    async fn subscribe_to_notifications(&mut self) -> Result<()> {
        let id = self.client.rpc_api().register_new_listener(ChannelConnection::new(
            "vprogs-l1-bridge",
            self.notification_channel.sender.clone(),
            ChannelType::Persistent,
        ));

        // Subscribe to chain changes (triggers v2 fetch) and finalization (pruning point).
        // Note: We don't need accepted_transaction_ids from VCC since we fetch via v2.
        for scope in [
            Scope::VirtualChainChanged(VirtualChainChangedScope::new(false)),
            Scope::PruningPointUtxoSetOverride(PruningPointUtxoSetOverrideScope {}),
        ] {
            self.client.rpc_api().start_notify(id, scope).await?;
        }

        Ok(())
    }
}

// ============================================================================
// Chain Update Handlers
// ============================================================================

impl BridgeWorker {
    /// Recovers the gap between `root` and `tip` by fetching chain block
    /// hashes without verbose data. This rebuilds the linked list so that rollback and
    /// finalization can walk the full chain.
    async fn recover_gap(&mut self, target: &ChainBlock) -> Result<()> {
        let start = self.virtual_chain.root();

        log::info!(
            "L1 bridge: recovering gap from index {} to index {}",
            start.index(),
            target.index(),
        );

        // Fetch chain block hashes without verbose data (no headers/transactions needed).
        let response =
            self.client.get_virtual_chain_from_block_v2(start.hash(), None, None).await?;

        let target_hash = target.hash();
        let mut found = false;

        for hash in response.added_chain_block_hashes.iter() {
            self.virtual_chain.add_block(*hash);
            if *hash == target_hash {
                found = true;
                break;
            }
        }

        if !found {
            return Err(Error::RecoveryTargetNotFound);
        }

        log::info!("L1 bridge: recovered gap up to index {}", self.virtual_chain.tip().index(),);
        Ok(())
    }

    /// Fetches and processes chain updates using the v2 API.
    ///
    /// This single method handles both initial sync and live updates:
    /// - Calls get_virtual_chain_from_block_v2 from our last checkpoint
    /// - Handles reorgs (removed blocks) if any
    /// - Emits events for added blocks with their accepted transactions
    async fn fetch_chain_updates(&mut self) -> Result<()> {
        // Start from the tip hash, or query the L1 pruning point if starting fresh
        // (sentinel root at index 0 means no blocks have been processed yet).
        let tip = self.virtual_chain.tip();
        let start_hash = if tip.index() == 0 {
            self.client.get_block_dag_info().await?.pruning_point_hash
        } else {
            tip.hash()
        };

        let response = self
            .client
            .get_virtual_chain_from_block_v2(start_hash, Some(RpcDataVerbosityLevel::High), None)
            .await?;

        // Handle reorg if there are removed blocks.
        if !response.removed_chain_block_hashes.is_empty() {
            self.handle_reorg(&response)?;
        }

        // Process added blocks.
        if response.added_chain_block_hashes.is_empty() {
            log::debug!("L1 bridge: no new blocks");
            return Ok(());
        }

        log::info!(
            "L1 bridge: processing {} new chain blocks",
            response.chain_block_accepted_transactions.len()
        );

        for acd in response.chain_block_accepted_transactions.iter() {
            let hash = acd.chain_block_header.hash.expect("hash missing despite High verbosity");
            let index = self.virtual_chain.add_block(hash);
            self.push_event(L1Event::ChainBlockAdded {
                index,
                header: Box::new(acd.chain_block_header.clone()),
                accepted_transactions: acd.accepted_transactions.clone(),
            });
        }

        Ok(())
    }

    /// Handles a reorg by rolling back and emitting a Rollback event.
    fn handle_reorg(&mut self, response: &GetVirtualChainFromBlockV2Response) -> Result<()> {
        let num_removed = response.removed_chain_block_hashes.len() as u64;
        let rollback_index = self.virtual_chain.rollback(num_removed)?;

        log::info!(
            "L1 bridge: reorg detected, {} blocks removed, rolling back to index {}",
            num_removed,
            rollback_index
        );
        self.push_event(L1Event::Rollback(rollback_index));
        Ok(())
    }

    /// Handles a sync result, logging and escalating errors as appropriate.
    fn handle_sync_result(&mut self, result: Result<()>) {
        if let Err(e) = result {
            if e.is_fatal() {
                self.fatal_error(e.to_string());
            } else {
                log::warn!("L1 bridge: sync failed, will retry on reconnect: {}", e);
            }
        }
    }

    /// Handles finalization (pruning point advancement).
    async fn handle_finalization(&mut self) -> Result<()> {
        let pruning_hash = self.client.get_block_dag_info().await?.pruning_point_hash;

        if let Some(new_root) = self.virtual_chain.advance_root(&pruning_hash)? {
            log::info!(
                "L1 bridge: pruning point advanced to index {} (hash {})",
                new_root.index(),
                pruning_hash
            );
            self.push_event(L1Event::Finalized(new_root));
        }

        Ok(())
    }
}
