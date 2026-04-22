use std::{collections::HashMap, sync::Arc, time::Duration};

use crossbeam_queue::SegQueue;
use futures::{FutureExt, select_biased};
use kaspa_notify::scope::{PruningPointUtxoSetOverrideScope, Scope, VirtualChainChangedScope};
use kaspa_rpc_core::{
    GetVirtualChainFromBlockV2Response, Notification,
    RpcDataVerbosityLevel::{Full, Low},
    api::{ctl::RpcState, rpc::RpcApi},
};
use kaspa_seq_commit::{
    hashing::{
        ActivityDigestBuilder, activity_leaf, lane_key, lane_tip_next, mergeset_context_hash,
    },
    types::{LaneTipInput, MergesetContext},
};
use kaspa_wrpc_client::prelude::*;
use tokio::sync::Notify;
use vprogs_core_types::Checkpoint;
use vprogs_l1_types::{ChainBlockMetadata, Hash, L1Transaction};
use workflow_core::channel::{Channel, MultiplexerChannel};

use crate::{
    L1BridgeConfig, L1Event,
    error::{Error, Result},
    reorg_filter::ReorgFilter,
    virtual_chain::VirtualChain,
};

/// Runs inside a dedicated thread and communicates with the L1 node over RPC. Pushes [`L1Event`]s
/// to a shared queue for the [`L1Bridge`] consumer.
pub(crate) struct BridgeWorker {
    /// RPC client for L1 communication.
    client: Arc<KaspaRpcClient>,
    /// Local view of the selected parent chain.
    virtual_chain: VirtualChain,
    /// Event queue shared with the bridge consumer.
    queue: Arc<SegQueue<L1Event>>,
    /// Wakes the consumer after pushing an event.
    event_signal: Arc<Notify>,
    /// Signals the worker to shut down.
    shutdown: Arc<Notify>,
    /// Receives L1 chain notifications (VCC, pruning point).
    notification_channel: Channel<Notification>,
    /// Receives RPC connection state changes.
    rpc_ctl_channel: MultiplexerChannel<RpcState>,
    /// Set to `true` on fatal errors to break out of the event loop.
    fatal: bool,
    /// When resuming with both root and tip set, holds the tip checkpoint until the chain between
    /// root and tip is backfilled on first connect.
    backfill_target: Option<Checkpoint<ChainBlockMetadata>>,
    /// Filters shallow reorgs based on accumulated depth.
    reorg_filter: ReorgFilter,
    /// Cache of recently-seen block hashes to their header timestamps. Pruned at finalization.
    timestamps: HashMap<Hash, u64>,
    /// If `Some`, filter emitted transactions to this subnetwork.
    subnetwork_filter: Option<[u8; 20]>,
    /// Lane key used when chaining lane tips. `None` disables lane-tip tracking.
    lane_key: Option<Hash>,
    /// Running lane tip across emitted chain blocks. Seeded from the resume tip.
    last_lane_tip: Hash,
}

impl BridgeWorker {
    /// Connects to the L1 node and runs the event loop until shutdown or a fatal error.
    ///
    /// If connection fails, pushes a [`L1Event::Fatal`] and returns immediately.
    pub(crate) async fn spawn(
        config: &L1BridgeConfig,
        queue: Arc<SegQueue<L1Event>>,
        event_signal: Arc<Notify>,
        shutdown: Arc<Notify>,
    ) {
        // Prefer root, fall back to tip, or default to a sentinel at index 0.
        let root_checkpoint = config.root.clone().or(config.tip.clone()).unwrap_or_default();
        let last_lane_tip = Hash::from_bytes(root_checkpoint.metadata().lane_tip);
        let virtual_chain = VirtualChain::new(root_checkpoint);

        let lane_key = config.subnetwork_id.map(|id| lane_key(&id));

        // If both root and tip are provided and differ, we need to backfill the chain between them
        // on first connect (lightweight, non-verbose sync).
        let backfill_target = match (&config.root, &config.tip) {
            (Some(root), Some(tip)) if root.metadata().hash != tip.metadata().hash => {
                Some(tip.clone())
            }
            _ => None,
        };

        // Use the public resolver when no explicit URL is given.
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
                return;
            }
        };

        // Subscribe to RPC state changes before connecting so we don't miss the initial Connected
        // event.
        let rpc_ctl_channel = client.rpc_ctl().multiplexer().channel();

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
            return;
        }

        Self {
            client,
            virtual_chain,
            queue,
            event_signal,
            shutdown,
            notification_channel: Channel::unbounded(),
            rpc_ctl_channel,
            fatal: false,
            backfill_target,
            reorg_filter: ReorgFilter::new(config.filter_half_life),
            timestamps: HashMap::new(),
            subnetwork_filter: config.subnetwork_id,
            lane_key,
            last_lane_tip,
        }
        .run()
        .await;
    }

    /// Priority-based event loop: shutdown > RPC state > chain notifications.
    async fn run(mut self) {
        loop {
            if self.fatal {
                log::error!("L1 bridge: stopping due to fatal error");
                break;
            }

            // Priority: shutdown > connection state > chain notifications.
            select_biased! {
                _ = self.shutdown.notified().fuse() => {
                    log::info!("L1 bridge shutdown requested");
                    break;
                }

                msg = self.rpc_ctl_channel.receiver.recv().fuse() => {
                    match msg {
                        Ok(RpcState::Connected) => self.handle_connected().await,
                        Ok(RpcState::Disconnected) => self.handle_disconnected(),
                        Err(e) => {
                            self.fatal_error(format!("RPC control channel closed: {}", e));
                        }
                    }
                }

                notification = self.notification_channel.receiver.recv().fuse() => {
                    let result = match notification {
                        Ok(Notification::VirtualChainChanged(_)) => {
                            self.fetch_chain_updates().await
                        }
                        Ok(Notification::PruningPointUtxoSetOverride(_)) => {
                            self.handle_finalization().await
                        }
                        Ok(_) => Ok(()),
                        Err(e) => Err(Error::ChannelClosed(e.to_string())),
                    };

                    self.handle_sync_result(result);
                }
            }
        }

        // Clean up the RPC connection before exiting.
        let _ = self.client.disconnect().await;
        log::info!("L1 bridge worker stopped");
    }

    // ========================================================================
    // Private helpers
    // ========================================================================

    /// Pushes an event and wakes the consumer.
    fn push_event(&self, event: L1Event) {
        self.queue.push(event);
        self.event_signal.notify_one();
    }

    /// Pushes a fatal event and flags the worker for shutdown.
    fn fatal_error(&mut self, reason: String) {
        log::error!("L1 bridge fatal error: {}", reason);
        self.push_event(L1Event::Fatal { reason });
        self.fatal = true;
    }

    /// Logs or escalates a sync result depending on whether the error is fatal.
    fn handle_sync_result(&mut self, result: Result<()>) {
        if let Err(e) = result {
            if e.is_fatal() {
                self.fatal_error(e.to_string());
            } else {
                log::warn!("L1 bridge: sync failed, will retry on reconnect: {}", e);
            }
        }
    }

    /// Called on RPC connect: subscribes to notifications, backfills the chain if resuming, then
    /// syncs to the current chain state.
    async fn handle_connected(&mut self) {
        log::info!("L1 bridge connected to {}", self.client.url().unwrap_or_default());

        // Step 1: Subscribe to chain notifications.
        if let Err(e) = self.subscribe_to_notifications().await {
            self.fatal_error(format!("failed to subscribe to notifications: {}", e));
            return;
        }

        // Step 2: If resuming, backfill the chain between root and tip.
        if let Some(target) = self.backfill_target.take() {
            let result = self.backfill_chain(&target).await;
            self.handle_sync_result(result);
            if self.fatal {
                return;
            }
        }

        // Notify consumer only after backfill succeeds.
        self.push_event(L1Event::Connected);

        // Step 3: Sync to the current chain state.
        let result = self.fetch_chain_updates().await;
        self.handle_sync_result(result);
    }

    /// Notifies the consumer that the connection was lost.
    fn handle_disconnected(&mut self) {
        log::info!("L1 bridge disconnected");
        self.push_event(L1Event::Disconnected);
    }

    /// Registers a notification listener for VirtualChainChanged (used as a "something changed"
    /// signal - actual data is fetched via the v2 API) and PruningPointUtxoSetOverride
    /// (finalization).
    async fn subscribe_to_notifications(&mut self) -> Result<()> {
        // Register a persistent listener that pipes notifications into our channel.
        let id = self.client.rpc_api().register_new_listener(ChannelConnection::new(
            "vprogs-l1-bridge",
            self.notification_channel.sender.clone(),
            ChannelType::Persistent,
        ));

        // VCC is subscribed without accepted_transaction_ids - we only use it as a "something
        // changed" signal and fetch verbose data via the v2 API.
        for scope in [
            Scope::VirtualChainChanged(VirtualChainChangedScope::new(false)),
            Scope::PruningPointUtxoSetOverride(PruningPointUtxoSetOverrideScope {}),
        ] {
            self.client.rpc_api().start_notify(id, scope).await?;
        }

        Ok(())
    }

    /// Backfills the chain between root and `target`. Only runs once on first connect when resuming
    /// with a saved root/tip pair.
    async fn backfill_chain(&mut self, target: &Checkpoint<ChainBlockMetadata>) -> Result<()> {
        let start = self.virtual_chain.root();

        log::info!(
            "L1 bridge: backfilling chain from index {} to index {}",
            start.index(),
            target.index(),
        );

        // Fetch with Low verbosity - sufficient for hash and blue_score needed for chain blocks.
        let response = self
            .client
            .get_virtual_chain_from_block_v2(start.metadata().hash, Some(Low), None)
            .await?;

        // Walk the chain block accepted transactions to get both hash and blue_score.
        let target_hash = target.metadata().hash;
        let mut found = false;

        for chain_block in response.chain_block_accepted_transactions.iter() {
            let hash = chain_block.chain_block_header.hash.unwrap_or_default();
            let blue_score = chain_block.chain_block_header.blue_score.unwrap_or(0);
            // Low verbosity: only hash and blue_score are populated here; remaining fields default.
            self.virtual_chain.advance_tip(ChainBlockMetadata {
                hash,
                blue_score,
                ..Default::default()
            });
            if hash == target_hash {
                found = true;
                break;
            }
        }

        if !found {
            return Err(Error::BackfillTargetNotFound(target_hash));
        }

        log::info!("L1 bridge: backfill complete up to index {}", self.virtual_chain.tip().index());
        Ok(())
    }

    /// Fetches chain updates from the current tip (or the L1 pruning point on first sync). Handles
    /// reorgs and emits `ChainBlockAdded` events.
    async fn fetch_chain_updates(&mut self) -> Result<()> {
        let tip = self.virtual_chain.tip();

        // Index 0 is the sentinel - no blocks processed yet, start from the L1 pruning point.
        let start_hash = if tip.index() == 0 {
            self.client.get_block_dag_info().await?.pruning_point_hash
        } else {
            tip.metadata().hash
        };

        // Fetch with Full verbosity to get complete headers and accepted transactions.
        let response = self
            .client
            .get_virtual_chain_from_block_v2(start_hash, Some(Full), self.reorg_filter.threshold())
            .await?;

        // Removed hashes indicate a reorg - roll back before processing additions.
        if !response.removed_chain_block_hashes.is_empty() {
            self.handle_reorg(&response)?;
        }

        log::info!(
            "L1 bridge: processing {} new chain blocks",
            response.chain_block_accepted_transactions.len()
        );

        // Extend the virtual chain and emit an event for each new block.
        for chain_block in response.chain_block_accepted_transactions.iter() {
            let hash = chain_block.chain_block_header.hash.expect("missing hash");
            let blue_score = chain_block.chain_block_header.blue_score.expect("missing blue_score");
            let daa_score = chain_block.chain_block_header.daa_score.expect("missing daa_score");
            let timestamp = chain_block.chain_block_header.timestamp.expect("missing timestamp");

            // Selected parent on the chain stream is the current virtual-chain tip.
            let selected_parent_hash = self.virtual_chain.tip().metadata().hash;
            let prev_timestamp = self.resolve_timestamp(selected_parent_hash, timestamp).await?;

            self.timestamps.insert(hash, timestamp);

            // Enumerate before filtering so kept txs retain their block-wide positions.
            let accepted_transactions: Vec<(u32, L1Transaction)> = chain_block
                .accepted_transactions
                .iter()
                .enumerate()
                .filter_map(|(idx, tx)| {
                    let tx = L1Transaction::try_from(tx.clone()).expect("missing tx fields");
                    match self.subnetwork_filter.as_ref() {
                        Some(want) if tx.subnetwork_id.as_bytes() != want => None,
                        _ => Some((idx as u32, tx)),
                    }
                })
                .collect();

            // With no lane_key both prev and new lane tips stay at zero.
            let prev_lane_tip = self.last_lane_tip.as_bytes();
            let lane_tip = match self.lane_key.as_ref() {
                Some(lane_key) => {
                    let mut activity = ActivityDigestBuilder::new();
                    for (merge_idx, tx) in &accepted_transactions {
                        activity.add_leaf(activity_leaf(&tx.id(), tx.version, *merge_idx));
                    }
                    let context_hash = mergeset_context_hash(&MergesetContext {
                        timestamp: prev_timestamp,
                        daa_score,
                        blue_score,
                    });
                    self.last_lane_tip = lane_tip_next(&LaneTipInput {
                        parent_ref: &self.last_lane_tip,
                        lane_key,
                        activity_digest: &activity.finalize(),
                        context_hash: &context_hash,
                    });
                    self.last_lane_tip.as_bytes()
                }
                None => [0; 32],
            };

            let lane_key = self.lane_key.as_ref().map_or([0; 32], |k| k.as_bytes());
            let metadata = ChainBlockMetadata {
                hash,
                blue_score,
                daa_score,
                timestamp,
                prev_timestamp,
                lane_key,
                prev_lane_tip,
                lane_tip,
            };
            let checkpoint = self.virtual_chain.advance_tip(metadata);

            self.push_event(L1Event::ChainBlockAdded {
                checkpoint,
                header: Box::new(chain_block.chain_block_header.clone()),
                accepted_transactions,
            });
        }

        Ok(())
    }

    /// Returns the header timestamp of `hash`, falling back to a one-shot `get_block` RPC
    /// lookup on cache miss. For the default-hash sentinel, returns `fallback`.
    async fn resolve_timestamp(&mut self, hash: Hash, fallback: u64) -> Result<u64> {
        if hash == Hash::default() {
            return Ok(fallback);
        }
        if let Some(&ts) = self.timestamps.get(&hash) {
            return Ok(ts);
        }
        // Cache miss - fetch the header (without transactions).
        let block = self.client.get_block(hash, false).await?;
        let ts = block.header.timestamp;
        self.timestamps.insert(hash, ts);
        Ok(ts)
    }

    /// Rolls back the virtual chain, updates the reorg filter, and emits a `Rollback` event.
    fn handle_reorg(&mut self, response: &GetVirtualChainFromBlockV2Response) -> Result<()> {
        let num_removed = response.removed_chain_block_hashes.len() as u64;
        let (checkpoint, blue_score_depth) = self.virtual_chain.rollback(num_removed)?;
        self.reorg_filter.record(blue_score_depth);
        self.last_lane_tip = Hash::from_bytes(checkpoint.metadata().lane_tip);

        log::info!(
            "L1 bridge: reorg detected, {} blocks removed, rolling back to index {} \
             (blue score depth: {}, filter threshold: {:?})",
            num_removed,
            checkpoint.index(),
            blue_score_depth,
            self.reorg_filter.threshold(),
        );
        self.push_event(L1Event::Rollback { checkpoint, blue_score_depth });
        Ok(())
    }

    /// Advances the root to the L1 pruning point and emits a `Finalized` event.
    async fn handle_finalization(&mut self) -> Result<()> {
        let pruning_hash = self.client.get_block_dag_info().await?.pruning_point_hash;

        if let Some(new_root) = self.virtual_chain.advance_root(&pruning_hash)? {
            log::info!(
                "L1 bridge: pruning point advanced to index {} (hash {})",
                new_root.index(),
                pruning_hash
            );
            // Drop all cached timestamps except the current tip.
            let tip_hash = self.virtual_chain.tip().metadata().hash;
            self.timestamps.retain(|&hash, _| hash == tip_hash);
            self.push_event(L1Event::Finalized(new_root));
        }

        Ok(())
    }
}
