use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use crossbeam_queue::SegQueue;
use futures::{FutureExt, select_biased};
use kaspa_consensus_core::subnets::SubnetworkId;
use kaspa_notify::scope::{PruningPointUtxoSetOverrideScope, Scope, VirtualChainChangedScope};
use kaspa_rpc_core::{
    GetVirtualChainFromBlockV2Response, Notification,
    RpcDataVerbosityLevel::Full,
    api::{ctl::RpcState, rpc::RpcApi},
};
use kaspa_seq_commit::{
    hashing::{
        ActivityDigestBuilder, activity_leaf, lane_key, lane_tip_next, mergeset_context_hash,
    },
    types::{LaneTipInput, MergesetContext},
};
use kaspa_wrpc_client::prelude::*;
use tokio::sync::{Notify, mpsc, watch};
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_core_types::{AccessMetadata, ChainSink, SchedulerTransaction};
use vprogs_l1_types::{
    ChainBlockMetadata, Hash, L1Transaction, L1TransactionCovenantExt, SettlementInfo,
};
use workflow_core::channel::{Channel, MultiplexerChannel};

use crate::{
    Command, L1BridgeConfig, L1Event,
    error::{Error, Result},
    reorg_filter::ReorgFilter,
};

/// Bounded retries for the virtual-chain RPC before surfacing the error. A real testnet node times
/// out transiently, so the bridge must ride out a blip rather than fatal on the first one. Sized to
/// cover a short node hiccup without wedging the worker indefinitely on a genuinely dead node.
const RPC_RETRY_MAX_ATTEMPTS: u32 = 10;
/// Delay between virtual-chain RPC retries.
const RPC_RETRY_DELAY: Duration = Duration::from_millis(500);

/// Bridges an L1 node's chain to a [`ChainSink`] over RPC, high-pass filtering reorgs.
pub(crate) struct BridgeWorker<T: ChainSink<ChainBlockMetadata, L1Transaction>> {
    /// RPC client for L1 communication.
    client: Arc<KaspaRpcClient>,
    /// The chain sink, driven directly. Owns the canonical chain.
    sink: T,
    /// L1-anchor metadata: the first block's parent while the sink's chain is empty (tip `0`).
    genesis: ChainBlockMetadata,
    /// API commands to apply against the sink, interleaved with L1 processing.
    api_requests: mpsc::Receiver<Command<T>>,
    /// Bridge events for observers.
    events: Arc<SegQueue<L1Event>>,
    /// Wakes observers after pushing an event.
    event_signal: Arc<Notify>,
    /// Lock-free shutdown signal.
    shutdown: Arc<AtomicAsyncLatch>,
    /// Receives L1 chain notifications (VCC, pruning point).
    notification_channel: Channel<Notification>,
    /// Receives RPC connection state changes.
    rpc_ctl_channel: MultiplexerChannel<RpcState>,
    /// Set on fatal error or disconnect to break out of the event loop on the next iteration.
    stopping: bool,
    /// Filters shallow reorgs based on accumulated depth.
    reorg_filter: ReorgFilter,
    /// If `Some`, filter emitted transactions to this subnetwork.
    subnetwork_filter: Option<SubnetworkId>,
    /// Lane key used when chaining lane tips. `None` disables lane-tip tracking.
    lane_key: Option<Hash>,
    /// Blue-score window within which a lane stays active without new transactions.
    finality_depth: u64,
    /// Covenant id tracked by [`ChainBlockMetadata::last_settlement`], or `None` to disable.
    covenant_id: Option<Hash>,
    /// Fresh-chain seed depth below the sink; `None` seeds from the pruning point.
    seed_depth: Option<u64>,
    /// On a fresh chain, seed the root at this explicit block instead of the sink or the pruning
    /// point. Takes precedence over `seed_depth`. `None` defers to `seed_depth`/pruning point.
    start_from: Option<Hash>,
    /// Optional observer the latest chain-block DAA score is published to, for catch-up progress.
    tip_daa: Option<Arc<AtomicU64>>,
    /// Optional `watch` sender the tip's last covenant settlement is published to (the bridge is
    /// the single writer), so each settler can read the canonical settlement without a confirm
    /// RTT.
    settlement: Option<watch::Sender<Option<SettlementInfo>>>,
}

impl<T: ChainSink<ChainBlockMetadata, L1Transaction>> BridgeWorker<T> {
    /// Connects to the L1 node and runs the event loop until shutdown or a fatal error.
    pub(crate) async fn spawn(
        config: L1BridgeConfig,
        sink: T,
        api_requests: mpsc::Receiver<Command<T>>,
        events: Arc<SegQueue<L1Event>>,
        event_signal: Arc<Notify>,
        shutdown: Arc<AtomicAsyncLatch>,
    ) {
        let lane_key = config.subnetwork_id.as_ref().map(|id| lane_key(id.as_bytes()));

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
                events.push(L1Event::Fatal { reason });
                event_signal.notify_one();
                return;
            }
        };

        // Subscribe to RPC state changes before connecting so we don't miss the Connected event.
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
            events.push(L1Event::Fatal { reason });
            event_signal.notify_one();
            return;
        }

        Self {
            client,
            sink,
            genesis: ChainBlockMetadata::default(),
            api_requests,
            events,
            event_signal,
            shutdown,
            notification_channel: Channel::unbounded(),
            rpc_ctl_channel,
            stopping: false,
            reorg_filter: ReorgFilter::new(config.filter_half_life),
            subnetwork_filter: config.subnetwork_id,
            lane_key,
            finality_depth: config.finality_depth,
            covenant_id: config.covenant_id,
            seed_depth: config.seed_depth,
            start_from: config.start_from,
            tip_daa: config.tip_daa.clone(),
            settlement: config.settlement_observer.clone(),
        }
        .run()
        .await;
    }

    /// Priority-based event loop: shutdown > API command > connection state > chain notifications.
    async fn run(mut self) {
        while !self.stopping {
            select_biased! {
                // Lock-free shutdown latch, checked first.
                _ = self.shutdown.wait().fuse() => {
                    log::info!("L1 bridge shutdown requested");
                    self.stopping = true;
                }

                // API command against the sink (e.g. pruning, reads needing &mut).
                cmd = self.api_requests.recv().fuse() => {
                    match cmd {
                        Some(cmd) => cmd(&mut self.sink),
                        // All API senders dropped: the node is gone, stop.
                        None => self.stopping = true,
                    }
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
                        Ok(other) => {
                            log::warn!("L1 bridge: ignoring unexpected notification: {:?}", other);
                            Ok(())
                        }
                        Err(e) => Err(Error::ChannelClosed(e.to_string())),
                    };

                    self.handle_sync_result(result);
                }
            }
        }

        // Clean up the RPC connection, then shut the sink down.
        let _ = self.client.disconnect().await;
        log::info!("L1 bridge worker stopped");
        self.sink.shutdown();
    }

    // ========================================================================
    // Private helpers
    // ========================================================================

    /// Pushes an event and wakes observers.
    fn push_event(&self, event: L1Event) {
        self.events.push(event);
        self.event_signal.notify_one();
    }

    /// Pushes a fatal event and flags the worker to stop.
    fn fatal_error(&mut self, reason: String) {
        log::error!("L1 bridge fatal error: {}", reason);
        self.push_event(L1Event::Fatal { reason });
        self.stopping = true;
    }

    /// Logs or escalates a sync result depending on whether the error is fatal.
    fn handle_sync_result(&mut self, result: Result<()>) {
        if let Err(e) = result {
            if e.is_fatal() {
                self.fatal_error(e.to_string());
            } else {
                log::warn!("L1 bridge: sync failed, will retry on next notification: {}", e);
            }
        }
    }

    /// Called on RPC connect: subscribes to notifications, establishes the tip, and syncs.
    async fn handle_connected(&mut self) {
        log::info!("L1 bridge connected to {}", self.client.url().unwrap_or_default());

        // Step 1: Subscribe to chain notifications.
        if let Err(e) = self.subscribe_to_notifications().await {
            self.fatal_error(format!("failed to subscribe to notifications: {}", e));
            return;
        }

        // Step 2: Establish the tip we thread from.
        let init_result = if self.sink.tip() > 0 {
            Ok(())
        } else {
            match (self.start_from, self.seed_depth) {
                (Some(hash), _) => self.seed_from_block(hash).await,
                (None, Some(depth)) => self.seed_from_recent(depth).await,
                (None, None) => self.seed_from_pruning_point().await,
            }
        };
        if let Err(e) = init_result {
            self.fatal_error(format!("chain init failed: {}", e));
            return;
        }

        // Step 3: publish the tip as a progress baseline, then announce Connected and sync.
        self.publish_tip_daa();
        self.publish_settlement();
        self.push_event(L1Event::Connected);
        let result = self.fetch_chain_updates().await;
        self.handle_sync_result(result);
    }

    /// The current canonical tip's metadata.
    fn tip_metadata(&self) -> ChainBlockMetadata {
        match self.sink.tip() {
            0 => self.genesis,
            id => self.sink.metadata(id).expect("canonical tip metadata is live"),
        }
    }

    /// Publishes the current tip's DAA score to the optional observer.
    fn publish_tip_daa(&self) {
        if let Some(observer) = &self.tip_daa {
            observer.store(self.tip_metadata().daa_score, Ordering::Relaxed);
        }
    }

    /// Publishes the tip's last covenant settlement to the optional `watch` sender, so each settler
    /// reads the canonical settlement. Sends `None` when the tip carries no settlement yet or a
    /// reorg has rolled past the last one. `send_replace` never errors, even with no live
    /// receivers, so a settler-less bridge still publishes harmlessly.
    fn publish_settlement(&self) {
        if let Some(sender) = &self.settlement {
            sender.send_replace(self.tip_metadata().last_settlement);
        }
    }

    /// Seeds the genesis anchor from the L1 pruning-point header.
    async fn seed_from_pruning_point(&mut self) -> Result<()> {
        let pruning_point = self
            .client
            .get_block(self.client.get_block_dag_info().await?.pruning_point_hash, false)
            .await?;

        self.genesis = (&pruning_point.header).into();

        Ok(())
    }

    /// Seeds the genesis anchor `depth` chain-blocks below the sink to start near the tip.
    ///
    /// `depth` is the reorg head-room; a deeper reorg panics in `rollback`.
    async fn seed_from_recent(&mut self, depth: u64) -> Result<()> {
        let sink = self.client.get_block_dag_info().await?.sink;

        // Lowest verbosity (no txs): we need each block's selected parent, then the landing header.
        let mut hash = sink;
        for _ in 0..depth {
            let parent = self
                .client
                .get_block(hash, false)
                .await?
                .verbose_data
                .expect("get_block returns verbose data")
                .selected_parent_hash;
            if parent == hash {
                break;
            }
            hash = parent;
        }

        let root = self.client.get_block(hash, false).await?;
        self.genesis = (&root.header).into();
        log::info!("L1 bridge: seeding {depth} blocks below sink {sink} (root {hash})");

        Ok(())
    }

    /// Seeds the virtual chain at an explicit block, installing it as the root/tip at index 0 so
    /// the first emitted block lands at index 1. Used to start replay at a known historical
    /// block (a covenant's deploy block) so a catch-up node rebuilds state forward from there.
    /// The block must be at or before the covenant bootstrap, or the bridge misses the
    /// bootstrap's settlements; a reorg below it panics in `rollback`, same as
    /// `seed_from_recent`.
    async fn seed_from_block(&mut self, hash: Hash) -> Result<()> {
        let block = self.client.get_block(hash, false).await?;
        self.genesis = (&block.header).into();
        log::info!("L1 bridge: seeding from explicit block {hash}");

        Ok(())
    }

    /// Notifies observers that the connection was lost and tears the worker down.
    fn handle_disconnected(&mut self) {
        log::info!("L1 bridge disconnected");
        self.push_event(L1Event::Disconnected);
        self.stopping = true;
    }

    /// Registers listeners for VirtualChainChanged and PruningPointUtxoSetOverride (finalization).
    async fn subscribe_to_notifications(&mut self) -> Result<()> {
        // Register a persistent listener that pipes notifications into our channel.
        let id = self.client.rpc_api().register_new_listener(ChannelConnection::new(
            "vprogs-l1-bridge",
            self.notification_channel.sender.clone(),
            ChannelType::Persistent,
        ));

        // VCC subscribed without accepted_transaction_ids - only a "something changed" trigger.
        for scope in [
            Scope::VirtualChainChanged(VirtualChainChangedScope::new(false)),
            Scope::PruningPointUtxoSetOverride(PruningPointUtxoSetOverrideScope {}),
        ] {
            self.client.rpc_api().start_notify(id, scope).await?;
        }

        Ok(())
    }

    /// Fetches the virtual chain from `from` with Full verbosity, retrying transient RPC failures
    /// with a bounded backoff. A real testnet node times out transiently; without this the chain
    /// init backfill (and steady-state follow) would fatal the whole worker on a single blip.
    ///
    /// Only transient `Error::Rpc` failures are retried. Terminal errors (`CheckpointLost`,
    /// `BackfillTargetNotFound`, and the rest) are returned immediately so a genuine reorg/prune
    /// past the root still fatals. The backoff sleep is interruptible by the worker's shutdown
    /// signal, so shutdown during a backfill is honored within one `RPC_RETRY_DELAY`.
    async fn get_vcc_with_retry(
        client: Arc<KaspaRpcClient>,
        shutdown: Arc<AtomicAsyncLatch>,
        from: Hash,
        threshold: Option<u64>,
    ) -> Result<GetVirtualChainFromBlockV2Response> {
        for attempt in 1..=RPC_RETRY_MAX_ATTEMPTS {
            match client
                .get_virtual_chain_from_block_v2(from, Some(Full), threshold)
                .await
                .map_err(Error::from)
            {
                Ok(response) => return Ok(response),
                Err(e) if e.is_fatal() || attempt == RPC_RETRY_MAX_ATTEMPTS => return Err(e),
                Err(e) => {
                    log::warn!(
                        "L1 bridge: get_virtual_chain_from_block_v2 failed \
                         (attempt {attempt}/{RPC_RETRY_MAX_ATTEMPTS}, retrying): {e}"
                    );
                    // Sleep, but let shutdown cut the backoff short.
                    select_biased! {
                        _ = shutdown.wait().fuse() => {
                            return Err(Error::ChannelClosed("shutdown during RPC retry".into()));
                        }
                        _ = tokio::time::sleep(RPC_RETRY_DELAY).fuse() => {}
                    }
                }
            }
        }
        unreachable!("retry loop returns on the final attempt")
    }

    /// Fetches chain updates from the current tip, handling reorgs and scheduling each new block.
    async fn fetch_chain_updates(&mut self) -> Result<()> {
        // Fetch with Full verbosity to get complete headers and accepted transactions. Resolve the
        // tip and the (mutably-computed) reorg threshold first so the shared borrow taken by the
        // retry helper doesn't overlap them.
        let from = self.tip_metadata().hash;
        let threshold = self.reorg_filter.threshold();
        let response =
            Self::get_vcc_with_retry(self.client.clone(), self.shutdown.clone(), from, threshold)
                .await?;

        // Removed hashes indicate a reorg - roll back before processing additions.
        if !response.removed_chain_block_hashes.is_empty() {
            self.handle_reorg(&response)?;
        }

        // Emit log for progress tracing.
        if !response.chain_block_accepted_transactions.is_empty() {
            log::info!(
                "L1 bridge: processing {} new chain blocks",
                response.chain_block_accepted_transactions.len()
            );
        }

        // Schedule each new block, threading its parent from the current tip.
        for chain_block in response.chain_block_accepted_transactions.iter() {
            // Validate the peer-controlled header up front; an elided field fails the sync and
            // downstream code sees only the validated metadata.
            let block = ChainBlockMetadata::try_from(&chain_block.chain_block_header)
                .map_err(|field| Error::MalformedResponse(field.into()))?;

            // The block's parent; its `last_settlement` carries forward, updated per tx below.
            let parent = self.tip_metadata();
            let mut last_settlement = parent.last_settlement;

            // Enumerate before filtering so kept txs keep their block-wide positions.
            let mut txs: Vec<SchedulerTransaction<L1Transaction>> =
                Vec::with_capacity(chain_block.accepted_transactions.len());
            for (idx, tx) in chain_block.accepted_transactions.iter().enumerate() {
                // Same trust boundary as the header: elided tx fields fail the sync.
                let tx = L1Transaction::try_from(tx.clone())
                    .map_err(|e| Error::MalformedResponse(e.to_string()))?;

                // Carry forward the last settlement.
                if let Some(id) = self.covenant_id {
                    last_settlement =
                        tx.settlement_info(id, block.hash, block.daa_score).or(last_settlement);
                }

                // Parse access metadata; malformed = no dependencies and prover attests.
                match self.subnetwork_filter.as_ref() {
                    Some(want) if tx.subnetwork_id != *want => {}
                    _ => txs.push(SchedulerTransaction::new(
                        idx as u32,
                        AccessMetadata::decode_vec(&mut tx.payload.as_slice()).unwrap_or_default(),
                        tx,
                    )),
                }
            }

            // Determine the lane tip over the block's accepted txs.
            let (lane_tip, lane_blue_score, lane_expired) = self.lane_state(&parent, &txs, &block);

            // Append the block's parent-threaded metadata and its txs to the sink.
            self.sink.append(
                ChainBlockMetadata {
                    parent_id: self.sink.tip(),
                    prev_seq_commit: parent.seq_commit,
                    lane_key: self.lane_key.unwrap_or_default(),
                    prev_timestamp: parent.timestamp,
                    prev_lane_tip: parent.lane_tip,
                    prev_lane_blue_score: parent.lane_blue_score,
                    lane_blue_score,
                    lane_tip,
                    lane_expired,
                    last_settlement,
                    ..block
                },
                txs,
            );
        }

        // Publish the batch's new tip so the progress reporter advances as catch-up proceeds.
        self.publish_tip_daa();
        self.publish_settlement();

        Ok(())
    }

    /// Returns the next `(lane_tip, lane_blue_score, lane_expired)` for this block.
    fn lane_state(
        &self,
        parent: &ChainBlockMetadata,
        txs: &[SchedulerTransaction<L1Transaction>],
        block: &ChainBlockMetadata,
    ) -> (Hash, u64, bool) {
        // Check whether the lane has gone silent past the finality window and needs to reset.
        let blue_score = block.blue_score;
        let lane_expired = blue_score.saturating_sub(parent.lane_blue_score) > self.finality_depth;

        // No lane configured or no activity this block -> carry parent state forward unchanged.
        let Some(lane_key) = self.lane_key.as_ref().filter(|_| !txs.is_empty()) else {
            return (parent.lane_tip, parent.lane_blue_score, lane_expired);
        };

        // Merkle root over this block's activity leaves.
        let mut activity = ActivityDigestBuilder::new();
        for tx in txs {
            activity.add_leaf(activity_leaf(&tx.tx.id(), tx.tx.version, tx.merge_idx));
        }

        // Context hash of the current chain block.
        let context_hash = mergeset_context_hash(&MergesetContext {
            timestamp: parent.timestamp,
            daa_score: block.daa_score,
            blue_score,
        });

        // Construct the new lane tip.
        let parent_ref = if lane_expired { parent.seq_commit } else { parent.lane_tip };
        let tip = lane_tip_next(&LaneTipInput {
            lane_key,
            parent_ref: &parent_ref,
            activity_digest: &activity.finalize(),
            context_hash: &context_hash,
        });

        (tip, blue_score, lane_expired)
    }

    /// Rolls the sink back to the surviving block, recording the reorg's depth in the filter.
    fn handle_reorg(&mut self, response: &GetVirtualChainFromBlockV2Response) -> Result<()> {
        // `removed` is tip-first: fork child is its last entry; an unknown one is below finality.
        let fork_child_hash = response.removed_chain_block_hashes.last().expect("non-empty reorg");
        let Some(fork_child) = self.sink.id(&fork_child_hash.as_bytes()) else {
            return Err(Error::ReorgBelowFinality(*fork_child_hash));
        };
        let new_tip = self.sink.metadata(fork_child).expect("a known id has metadata").parent_id;

        // Blue-score depth (old tip minus new tip) feeds the reorg filter.
        let old_blue = self.sink.metadata(self.sink.tip()).map_or(0, |m| m.blue_score);
        let new_blue = self.sink.metadata(new_tip).map_or(0, |m| m.blue_score);
        let blue_score_depth = old_blue.saturating_sub(new_blue);
        self.reorg_filter.record(blue_score_depth);

        // Perform sink rollback.
        log::info!(
            "L1 bridge: reorg detected, {} blocks removed, rolling back to id {} \
             (blue score depth: {}, filter threshold: {:?})",
            response.removed_chain_block_hashes.len(),
            new_tip,
            blue_score_depth,
            self.reorg_filter.threshold(),
        );
        self.sink.rollback(new_tip);

        Ok(())
    }

    /// Advances finalization to the L1 pruning point: finalizes the sink's ids below it.
    async fn handle_finalization(&mut self) -> Result<()> {
        let pruning_hash = self.client.get_block_dag_info().await?.pruning_point_hash;

        if let Some(below) = self.sink.id(&pruning_hash.as_bytes()) {
            self.sink.finalize(below);
            log::info!("L1 bridge: pruning point advanced to id {} (hash {})", below, pruning_hash);
        }

        Ok(())
    }
}
