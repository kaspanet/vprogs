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
    RpcOptionalHeader,
    api::{ctl::RpcState, rpc::RpcApi},
};
use kaspa_seq_commit::{
    hashing::{
        ActivityDigestBuilder, activity_leaf, lane_key, lane_tip_next, mergeset_context_hash,
    },
    types::{LaneTipInput, MergesetContext},
};
use kaspa_wrpc_client::prelude::*;
use tokio::sync::{Notify, mpsc};
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_core_types::{AccessMetadata, ChainSink, SchedulerTransaction};
use vprogs_l1_types::{ChainBlockMetadata, Hash, L1Transaction, L1TransactionCovenantExt};
use workflow_core::channel::{Channel, MultiplexerChannel};

use crate::{
    L1BridgeConfig, L1Event,
    error::{Error, Result},
    reorg_filter::ReorgFilter,
};

/// A boxed command applied against the sink on the worker thread, which is the bridge's only
/// writer of `&mut` sink state (so L1 ingestion and API calls never contend).
pub type Command<T> = Box<dyn FnOnce(&mut T) + Send>;

/// Runs inside a dedicated thread, talks to the L1 node over RPC, and drives the sink directly.
///
/// The sink (a [`ChainSink`]) owns the canonical chain, so the bridge keeps no chain tracker of
/// its own: it threads the next block's parent by reading the tip metadata back through the sink,
/// holding only a genesis anchor for the empty-chain case. The worker also applies API commands
/// against the sink, interleaved with L1 processing. Only events (`Connected` / `Disconnected` /
/// `Fatal`) are pushed to a queue for observers; chain changes are direct sink calls.
pub(crate) struct BridgeWorker<T: ChainSink<ChainBlockMetadata, L1Transaction>> {
    /// RPC client for L1 communication.
    client: Arc<KaspaRpcClient>,
    /// The chain sink, driven directly. Owns the canonical chain.
    sink: T,
    /// L1-anchor metadata for the empty-chain case (sink tip `0`), threading the first block's
    /// parent fields. Set once at startup from L1; once the chain is non-empty the running tip is
    /// read from the sink (see `tip_metadata`).
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
    /// On a fresh chain, seed the root this many chain-blocks below the sink instead of the
    /// pruning point. `None` seeds from the pruning point.
    seed_depth: Option<u64>,
    /// Optional observer the latest chain-block DAA score is published to, for external progress
    /// reporting during catch-up.
    tip_daa: Option<Arc<AtomicU64>>,
}

impl<T: ChainSink<ChainBlockMetadata, L1Transaction>> BridgeWorker<T> {
    /// Connects to the L1 node and runs the event loop until shutdown or a fatal error.
    ///
    /// If connection fails, pushes a [`L1Event::Fatal`] and returns immediately.
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
            tip_daa: config.tip_daa.clone(),
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

        // Clean up the RPC connection, then shut the sink down (joins its workers + storage).
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

    /// Called on RPC connect: subscribes to notifications, establishes the threading tip, and
    /// syncs.
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
            match self.seed_depth {
                Some(depth) => self.seed_from_recent(depth).await,
                None => self.seed_from_pruning_point().await,
            }
        };
        if let Err(e) = init_result {
            self.fatal_error(format!("chain init failed: {}", e));
            return;
        }

        // Step 3: Notify observers and sync to current chain state. Publish the tip first so a
        // progress reporter has a baseline before the first (potentially large) batch lands.
        self.publish_tip_daa();
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

    /// Seeds the threading tip from the L1 pruning-point header, so the first emitted block extends
    /// a real parent (`parent_id` 0, since the sink's chain is still empty).
    async fn seed_from_pruning_point(&mut self) -> Result<()> {
        let pruning_point = self
            .client
            .get_block(self.client.get_block_dag_info().await?.pruning_point_hash, false)
            .await?;

        self.genesis = (&pruning_point.header).into();

        Ok(())
    }

    /// Seeds the threading tip `depth` chain-blocks below the current sink (instead of the pruning
    /// point), so the bridge starts near the tip rather than replaying the whole pruning window.
    /// Walks the selected-parent chain back `depth` blocks from the sink and installs the block it
    /// lands on as the tip.
    ///
    /// `depth` is the reorg head-room: a reorg shallower than it never rolls back past this tip. A
    /// deeper reorg does, and the sink panics in `rollback` - which means `depth` is
    /// configured too small for the network. The walk stops early if it reaches the chain's
    /// base (a block whose selected parent is itself) before `depth`, seeding from there.
    async fn seed_from_recent(&mut self, depth: u64) -> Result<()> {
        let sink = self.client.get_block_dag_info().await?.sink;

        // Lowest verbosity (no transactions): we only need each block's selected parent, then the
        // header of the block we land on.
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

    /// Notifies observers that the connection was lost and tears the worker down.
    fn handle_disconnected(&mut self) {
        log::info!("L1 bridge disconnected");
        self.push_event(L1Event::Disconnected);
        self.stopping = true;
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

    /// Fetches chain updates from the current tip, handling reorgs and scheduling each new block.
    async fn fetch_chain_updates(&mut self) -> Result<()> {
        // Fetch with Full verbosity to get complete headers and accepted transactions.
        let response = self
            .client
            .get_virtual_chain_from_block_v2(
                self.tip_metadata().hash,
                Some(Full),
                self.reorg_filter.threshold(),
            )
            .await?;

        // Removed hashes indicate a reorg - roll back before processing additions.
        if !response.removed_chain_block_hashes.is_empty() {
            self.handle_reorg(&response)?;
        }

        if !response.chain_block_accepted_transactions.is_empty() {
            log::info!(
                "L1 bridge: processing {} new chain blocks",
                response.chain_block_accepted_transactions.len()
            );
        }

        // Schedule each new block, threading its parent from the locally-held tip.
        for chain_block in response.chain_block_accepted_transactions.iter() {
            let header = &chain_block.chain_block_header;
            let parent_meta = self.tip_metadata();
            let block_hash = header.hash.expect("missing hash");
            let mut last_settlement = parent_meta.last_settlement;

            // Enumerate before filtering so kept txs retain their block-wide positions.
            let accepted_transactions: Vec<(u32, L1Transaction)> = chain_block
                .accepted_transactions
                .iter()
                .enumerate()
                .filter_map(|(idx, tx)| {
                    let tx = L1Transaction::try_from(tx.clone()).expect("missing tx fields");
                    if let Some(id) = self.covenant_id {
                        last_settlement = tx.settlement_info(id, block_hash).or(last_settlement);
                    }
                    match self.subnetwork_filter.as_ref() {
                        Some(want) if tx.subnetwork_id != *want => None,
                        _ => Some((idx as u32, tx)),
                    }
                })
                .collect();

            let (lane_tip, lane_blue_score, lane_expired) =
                self.advance_lane(&parent_meta, &accepted_transactions, header);

            let metadata = ChainBlockMetadata {
                // The sink's current canonical tip is the block this one extends.
                parent_id: self.sink.tip(),
                prev_seq_commit: parent_meta.seq_commit,
                lane_key: self.lane_key.unwrap_or_default(),
                prev_timestamp: parent_meta.timestamp,
                prev_lane_tip: parent_meta.lane_tip,
                prev_lane_blue_score: parent_meta.lane_blue_score,
                lane_blue_score,
                lane_tip,
                lane_expired,
                last_settlement,
                ..ChainBlockMetadata::try_from(header).unwrap()
            };

            // Pair each accepted tx with its declared resource accesses and hand the batch to the
            // sink, which assigns the never-reused id and processes it. Malformed access
            // metadata = no dependencies; the prover attests invalidity.
            let txs = accepted_transactions
                .into_iter()
                .map(|(idx, tx)| {
                    SchedulerTransaction::new(
                        idx,
                        AccessMetadata::decode_vec(&mut tx.payload.as_slice()).unwrap_or_default(),
                        tx,
                    )
                })
                .collect();
            self.sink.append(metadata, txs);
        }

        // Publish the batch's new tip so the progress reporter advances as catch-up proceeds.
        self.publish_tip_daa();

        Ok(())
    }

    /// Returns the next `(lane_tip, lane_blue_score, lane_expired)` for this block.
    fn advance_lane(
        &self,
        parent: &ChainBlockMetadata,
        accepted_transactions: &[(u32, L1Transaction)],
        header: &RpcOptionalHeader,
    ) -> (Hash, u64, bool) {
        // Check whether the lane has gone silent past the finality window and needs to reset.
        let blue_score = header.blue_score.expect("missing blue_score");
        let lane_expired = blue_score.saturating_sub(parent.lane_blue_score) > self.finality_depth;

        // No lane configured or no activity this block -> carry parent state forward unchanged.
        let Some(lane_key) = self.lane_key.as_ref().filter(|_| !accepted_transactions.is_empty())
        else {
            return (parent.lane_tip, parent.lane_blue_score, lane_expired);
        };

        let parent_ref = if lane_expired { parent.seq_commit } else { parent.lane_tip };

        // Merkle root over this block's activity leaves.
        let mut activity = ActivityDigestBuilder::new();
        for (merge_idx, tx) in accepted_transactions {
            activity.add_leaf(activity_leaf(&tx.id(), tx.version, *merge_idx));
        }

        // Context hash of the current chain block.
        let context_hash = mergeset_context_hash(&MergesetContext {
            timestamp: parent.timestamp,
            daa_score: header.daa_score.expect("missing daa_score"),
            blue_score,
        });

        // Construct the new lane tip.
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
