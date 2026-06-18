use std::sync::{Arc, Mutex, Weak};

use kaspa_addresses::{Address, Prefix, Version};
use kaspa_consensus::consensus::Consensus;
use kaspa_consensus_core::{
    api::ConsensusApi,
    constants::TX_VERSION_TOCCATA,
    mass::units::ComputeBudget,
    subnets::SubnetworkId,
    tx::{Transaction, TransactionOutpoint, TransactionQueryResult, TransactionType, UtxoEntry},
};
use kaspa_hashes::Hash;
use kaspa_seq_commit::hashing::lane_key;
use kaspa_txscript::standard::pay_to_script_hash_script;
use rand::{Rng, SeedableRng, rngs::StdRng};
use tempfile::TempDir;
use vprogs_core_atomics::{AsyncQueue, AtomicAsyncLatch};
use vprogs_core_smt::EMPTY_HASH;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ChainSink, ResourceId, SchedulerTransaction};
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_l1_wallet::{build, encode_activity_payload};
use vprogs_scheduling_scheduler::{Processor, Scheduler};
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_aggregate_prover::{AggregateProverConfig, ScheduledBundle, SettlementArtifact};
use vprogs_zk_backend_risc0_api::{Backend, ProofType, Receipt};
use vprogs_zk_backend_risc0_covenant::{Settlement, SettlementDevInput};
use vprogs_zk_backend_risc0_settler::{
    CovenantState, bootstrap_redeem, build_dev_settlement, build_settlement, dev_bootstrap_redeem,
};
use vprogs_zk_backend_risc0_test_suite::{
    batch_aggregator_elf, batch_processor_elf, build_scheduler, dev_mode_enabled,
    read_resource_u32, transaction_processor_elf,
};
use vprogs_zk_batch_prover::BatchProverConfig;
use vprogs_zk_vm::{ProvingPipeline, Vm};

use super::{DriverStats, L2Config};
use crate::{
    l2_miner::{ProduceCtx, Producer},
    lane_source::ConsensusLaneSource,
};

type Store = RocksDbStore;
type V = Vm<Backend, Store>;

/// Compute budget for the dev covenant input (dev redeem has no precompile; 100 covers it).
const DEV_COVENANT_BUDGET: ComputeBudget = ComputeBudget(100);

/// One selected-chain block the driver has scheduled, kept so reorgs can roll back exactly.
struct BlockRec {
    /// Block hash.
    hash: Hash,
    /// Canonical batch id the scheduler assigned this block.
    id: u64,
    /// Chain-block metadata recorded for this block.
    meta: ChainBlockMetadata,
    /// Number of lane transactions the driver scheduled into this block.
    lane_tx_count: u32,
}

/// A covenant state seen accepted on the current selected chain, tagged with the chain length right
/// after the block that confirmed it. A reorg that rolls the chain back below `marker` pops this
/// entry, so the covenant history always matches the live chain. `confirmed[0]` is the bootstrap.
struct ConfirmedCovenant {
    /// Chain length right after the block that confirmed this covenant.
    marker: usize,
    /// The confirmed covenant state.
    covenant: CovenantState,
}

/// A covenant transaction issued into a block but not yet seen accepted on the selected chain.
///
/// If its block loses the chain race (orphaned), the tx never lands; after [`REISSUE_DEADLINE`]
/// mined blocks the driver re-issues from the live confirmed tip. `basis_outpoint` is the covenant
/// UTXO this tx spends (None for the coinbase-funded bootstrap); when a reorg pops that basis off
/// `confirmed`, the pending tx is abandoned immediately rather than waiting for the deadline.
struct PendingCovenant {
    /// Id of the issued-but-unconfirmed covenant transaction.
    txid: Hash,
    /// Covenant UTXO this tx spends, or `None` for the coinbase-funded bootstrap.
    basis_outpoint: Option<TransactionOutpoint>,
    /// Chain index of the block this tx was issued into.
    issued_block_index: u64,
    /// Covenant state this tx advances to once confirmed.
    next: CovenantState,
    /// Whether this is the bootstrap (covenant-creating) transaction.
    is_bootstrap: bool,
}

/// Mined blocks a pending covenant tx may stay unconfirmed before the driver assumes its block was
/// orphaned and re-issues. Set well above the expected reorg depth so a re-issue only fires once
/// the orphaned branch is too deep to return (which would otherwise double-spend the basis).
const REISSUE_DEADLINE: u64 = 30;

/// If a covenant tx is re-issued this many times without any settlement landing, the covenant can
/// never make progress: a liveness failure. Panics with the seed so it is reproducible.
const MAX_REISSUES_WITHOUT_PROGRESS: u64 = 200;

/// The L2 execution stack: the zk `Vm`-backed scheduler over a temp-backed store. Held as a unit
/// so the real-proof mode can rebuild it once the covenant id is known (the batch prover binds the
/// covenant id into every journal, and it isn't known until the bootstrap is mined).
struct Exec {
    /// The zk `Vm`-backed scheduler.
    scheduler: Scheduler<Store, V>,
    /// Temp-backed state store the scheduler runs over.
    store: Store,
    /// Temp directory backing the store, kept alive for the store's lifetime.
    _db: TempDir,
    /// A clone of the processor (shares the proving pipeline) kept only so [`Drop`] can signal the
    /// batch-prover worker to shut down; the simulation drops the scheduler without calling
    /// `Scheduler::shutdown`, so the worker would otherwise loop forever. No-op when proving is
    /// off.
    proc_handle: V,
}

/// The L2 driver. Owns the execution stack + store and tracks the selected chain it has scheduled.
pub struct L2Driver {
    /// Subnetwork id the driver's lane activity transactions carry.
    lane_subnet: SubnetworkId,
    /// SMT lane key the driver's activity and settlements commit to.
    lane_key: Hash,
    /// Resource id whose per-block counter the driver tracks and asserts.
    tracked: ResourceId,
    /// Number of lane activity transactions issued per block.
    activity_per_block: u64,
    /// Blocks between settlement transactions.
    settle_every: u64,
    /// Number of batches the aggregate prover bundles per proof.
    bundle_size: usize,
    /// Seeded RNG driving deterministic activity and settlement choices.
    rng: StdRng,

    /// Proving backend shared with the execution stack.
    backend: Backend,
    /// The L2 execution stack (scheduler + store).
    exec: Exec,
    /// Weak handle to the node's consensus, used to (re)build the batch prover's lane source after
    /// bootstrap. Weak so it never extends the consensus lifetime (see [`ConsensusLaneSource`]).
    consensus: Weak<Consensus>,

    /// Whether the genesis/seed state has been installed yet.
    seeded: bool,
    /// Chain-block metadata the driver seeds its first block from.
    seed_meta: ChainBlockMetadata,
    /// Selected-chain blocks scheduled so far, kept for exact reorg rollback.
    chain: Vec<BlockRec>,
    /// Expected value of the tracked resource's counter, checked each block.
    expected_counter: u32,

    /// Whether the driver issues settlement transactions.
    settlements_enabled: bool,
    /// Proving-driven settlement mode: prove each bundle and settle it from the bundle's artifact,
    /// instead of the inline (no-prover) dev settlements. Implied by `enable_proving &&
    /// enable_settlements`. With real (CUDA) receipts each bundle settles via the production
    /// `Settlement::build` (`OpZkPrecompile`); under `RISC0_DEV_MODE` the receipts are stubs the
    /// precompile would reject, so it settles via the dev redeem (`build_dev_settlement`) instead.
    /// Single-miner only (a reorg can orphan a block whose batch the async worker is proving).
    real_e2e: bool,
    /// In `real_e2e`, false until the proving stack has been rebuilt with the live covenant id
    /// (after the bootstrap confirms); gates activity + settlement so nothing is proved against
    /// the placeholder covenant id. Always true in the other modes.
    proving_ready: bool,
    /// Set when the bootstrap confirms in `real_e2e`; the next `produce` rebuilds the proving
    /// stack.
    init_proving_pending: bool,
    /// Queue the in-process aggregate prover publishes each bundle handle onto (real_e2e only).
    /// The driver pops from it to settle proved bundles. `None` in the other modes and before
    /// the proving stack is built.
    settlement_queue: Option<AsyncQueue<ScheduledBundle<SettlementArtifact<Receipt>>>>,
    /// Batches submitted to the aggregate prover but not yet accounted by a bundle outcome
    /// (real_e2e only). Gates the settlement back-pressure and is reconciled by each outcome's
    /// `batches`.
    outstanding_batches: usize,

    /// Covenant states confirmed on the live chain (`confirmed[0]` = bootstrap); empty until the
    /// bootstrap lands. Popped on reorg, so the tip is always the live covenant.
    confirmed: Vec<ConfirmedCovenant>,
    /// The in-flight covenant tx awaiting acceptance, if any.
    pending: Option<PendingCovenant>,
    /// Re-issues since the last settlement landed; reset on progress, bounds liveness.
    reissues_since_progress: u64,

    /// Opened once the run is winding down (past the miner's block target) and the last settlement
    /// has confirmed, i.e. no settlement is left in flight. The miner watches it to stop mining
    /// drain blocks, so none is issued-but-unobserved when the run halts. One-shot: while draining
    /// the driver issues no new work, so `pending` only ever clears once.
    drained: AtomicAsyncLatch,

    /// Shared run statistics handed back to the caller.
    stats: Arc<Mutex<DriverStats>>,
}

/// Builds a fresh execution stack (temp store + zk `Vm` scheduler). When `proving` is set the `Vm`
/// drives the full proving stack (transaction + batch + aggregate provers), reading lane proofs
/// from `consensus` and binding `covenant_id` into every journal, and returns the queue the
/// aggregate prover publishes each bundle handle onto; otherwise it is execution-only and returns
/// `None`.
fn build_exec(
    backend: &Backend,
    lane: Hash,
    proving: bool,
    covenant_id: Option<Hash>,
    consensus: Weak<Consensus>,
) -> (Exec, Option<AsyncQueue<ScheduledBundle<SettlementArtifact<Receipt>>>>) {
    let db = tempfile::tempdir().expect("temp db dir");
    let store = RocksDbStore::open(db.path().join("l2"));
    let (pipeline, settlement_queue) = if proving {
        let queue = AsyncQueue::new();
        let pipeline = ProvingPipeline::aggregate(
            backend.clone(),
            store.clone(),
            BatchProverConfig { lane_key: lane, covenant_id },
            AggregateProverConfig {
                lane_key: lane,
                covenant_id,
                lane_source: ConsensusLaneSource::from_weak(consensus),
                settlement_queue: Some(queue.clone()),
                bundle_size: 1..=usize::MAX,
            },
        );
        (pipeline, Some(queue))
    } else {
        (ProvingPipeline::None, None)
    };
    let vm = Vm::new(backend.clone(), pipeline);
    let proc_handle = vm.clone();
    let scheduler = build_scheduler(vm, store.clone());
    (Exec { scheduler, store, _db: db, proc_handle }, settlement_queue)
}

impl L2Driver {
    /// Builds a driver with a fresh temp-backed store and a zk `Vm`. When `config.enable_proving`
    /// is set the `Vm` drives the real batch prover, reading lane proofs from `consensus` (the
    /// node this driver's miner runs); otherwise it is execution-only. Returns the driver and a
    /// shared stats handle the test can read after the run.
    pub fn new(
        config: L2Config,
        consensus: &Arc<Consensus>,
    ) -> (Self, Arc<Mutex<DriverStats>>, AtomicAsyncLatch) {
        let backend = Backend::new(
            &transaction_processor_elf(),
            &batch_processor_elf(),
            &batch_aggregator_elf(),
            ProofType::Succinct,
        );

        let lane_subnet = SubnetworkId::from_namespace(config.lane_id.to_be_bytes());
        let lane = lane_key(lane_subnet.as_bytes());
        let weak = Arc::downgrade(consensus);

        // Real-proof end-to-end: prove and settle from real receipts. Its proving stack is built
        // lazily (after bootstrap, with the real covenant id), so start execution-only. The
        // prove-only mode (proving without settlements) builds its batch pipeline now, binding the
        // zero placeholder covenant id since no on-chain settlement consumes those receipts.
        let real_e2e = config.enable_proving && config.enable_settlements;
        let prove_only = config.enable_proving && !config.enable_settlements;
        let (exec, settlement_queue) = build_exec(&backend, lane, prove_only, None, weak.clone());

        let stats = Arc::new(Mutex::new(DriverStats::default()));
        let drained = AtomicAsyncLatch::new();
        let driver = Self {
            lane_subnet,
            lane_key: lane,
            tracked: ResourceId::for_test(config.lane_id as usize),
            activity_per_block: config.activity_per_block,
            settle_every: config.settle_every.max(1),
            bundle_size: config.bundle_size.max(1),
            rng: StdRng::seed_from_u64(config.seed),
            backend,
            exec,
            consensus: weak,
            seeded: false,
            seed_meta: ChainBlockMetadata::default(),
            chain: Vec::new(),
            expected_counter: 0,
            settlements_enabled: config.enable_settlements,
            real_e2e,
            proving_ready: !real_e2e,
            init_proving_pending: false,
            settlement_queue,
            outstanding_batches: 0,
            confirmed: Vec::new(),
            pending: None,
            reissues_since_progress: 0,
            drained: drained.clone(),
            stats: stats.clone(),
        };
        (driver, stats, drained)
    }

    /// Rebuilds the execution stack with the real batch prover bound to the live covenant id, over
    /// a fresh store. Called once, right after the bootstrap confirms: the proven bundles' journals
    /// then commit the real covenant id (the on-chain script rejects the zero placeholder). The new
    /// store starts at the empty SMT, matching the bootstrap's `EMPTY_HASH` state, so the first
    /// proved bundle chains from the bootstrap. Execution/cursor state resets to follow the chain
    /// fresh from the current sink (only post-bootstrap activity is proved + settled).
    fn init_proving(&mut self) {
        let covenant_id = self.confirmed[0].covenant.covenant_id;
        let (exec, settlement_queue) = build_exec(
            &self.backend,
            self.lane_key,
            true,
            Some(covenant_id),
            self.consensus.clone(),
        );
        self.exec = exec;
        self.settlement_queue = settlement_queue;
        self.proving_ready = true;
        self.seeded = false;
        self.seed_meta = ChainBlockMetadata::default();
        self.chain.clear();
        self.expected_counter = 0;
        self.outstanding_batches = 0;
    }

    /// Follows the node's selected chain from the driver's cursor: rolls back on reorg, then
    /// schedules each new block's lane transactions, watches for covenant acceptance, and checks
    /// the counter invariant.
    fn catch_up(&mut self, c: &dyn ConsensusApi) {
        if !self.seeded {
            // Start from the current sink; only blocks mined afterwards are scheduled.
            let sink = c.get_sink();
            let hdr = c.get_header(sink).expect("seed header");
            self.seed_meta = base_meta(sink, &hdr, self.lane_key);
            self.seeded = true;
            return;
        }

        let low = self.chain.last().map(|b| b.hash).unwrap_or(self.seed_meta.hash);
        let path = c.get_virtual_chain_from_block(low, None).expect("virtual chain");

        if !path.removed.is_empty() {
            // Real-proof mode is single-miner only: a reorg would orphan a block whose batch the
            // async aggregate prover may already be bundling, desyncing `outstanding_batches` from
            // the prover's stream (which independently drops rolled-back batches). Fail loudly
            // rather than settle a bundle proved against a dead chain. See the aggregate prover's
            // rollback TODO for the framework-side fix (cancelling an in-flight bundle proof).
            assert!(
                !(self.real_e2e && self.proving_ready),
                "real-proof settlement requires a single miner; got a reorg",
            );
            let depth = path.removed.len() as u64;
            let keep = self.chain.len().saturating_sub(path.removed.len());
            for b in self.chain.drain(keep..) {
                self.expected_counter -= b.lane_tx_count;
            }
            let target_id = self.chain.last().map(|b| b.id).unwrap_or(0);
            self.exec.scheduler.rollback_to(target_id).expect("rollback");
            self.rollback_covenant(keep);
            let mut s = self.stats.lock().unwrap();
            s.reorgs += 1;
            s.max_reorg_depth = s.max_reorg_depth.max(depth);
        }

        for hash in path.added {
            let hdr = c.get_header(hash).expect("added header");
            let parent = self.chain.last().map(|b| &b.meta).unwrap_or(&self.seed_meta);
            let mut meta = child_meta(hash, &hdr, parent, self.lane_key);

            let accepted = match c
                .get_transactions_by_accepting_block(hash, None, TransactionType::Transaction)
                .expect("accepted txs")
            {
                TransactionQueryResult::Transaction(txs) => txs,
                TransactionQueryResult::SignableTransaction(_) => {
                    unreachable!("requested Transaction")
                }
            };

            let lane_txs: Vec<(u32, Transaction)> = accepted
                .iter()
                .enumerate()
                .filter(|(_, tx)| tx.subnetwork_id == self.lane_subnet)
                .map(|(idx, tx)| (idx as u32, tx.clone()))
                .collect();

            // Use the consensus's own lane tip (authoritative) when the lane saw activity.
            if !lane_txs.is_empty() {
                // First activation: nothing has been folded into the lane yet, so consensus's
                // resolver falls back to `prev_seq_commit`. The guest's `verify_activity` prefers
                // `prev_seq_commit` over `prev_lane_tip` only when `lane_expired`, so set it to
                // mirror that path (same workaround as `settlement_l1_e2e::settle_1`).
                if meta.prev_lane_blue_score == 0 {
                    meta.lane_expired = true;
                }
                if let Ok(proof) = c.get_seq_commit_lane_proof(hash, self.lane_key) {
                    if let Some(lane) = proof.lane {
                        meta.lane_tip = lane.tip;
                        meta.lane_blue_score = lane.blue_score;
                    }
                }
            }

            let sched_txs: Vec<SchedulerTransaction<Transaction>> = lane_txs
                .iter()
                .map(|(idx, tx)| {
                    let mut payload = tx.payload.as_slice();
                    let resources = AccessMetadata::decode_vec(&mut payload).unwrap_or_default();
                    SchedulerTransaction::new(*idx, resources, tx.clone())
                })
                .collect();

            let count = lane_txs.len() as u32;
            let batch = self.exec.scheduler.schedule(meta, sched_txs);
            let id = self.exec.scheduler.tip();
            batch.wait_committed_blocking();
            // In real-proof mode the batch was submitted to the aggregate prover (via the
            // scheduler); count it as outstanding so the settlement loop knows a bundle
            // outcome is coming. Other modes drop the batch (no settlement consumes the
            // receipt).
            if self.real_e2e && self.proving_ready {
                self.outstanding_batches += 1;
            }
            self.expected_counter += count;
            self.chain.push(BlockRec { hash, id, meta, lane_tx_count: count });

            // Core invariant: the decoded counter equals lane txs executed on this chain.
            let actual = read_resource_u32(&self.exec.store, self.tracked);
            assert_eq!(
                actual, self.expected_counter,
                "lane counter mismatch at block {hash}: expected {} got {}",
                self.expected_counter, actual,
            );

            // Confirm any covenant tx that landed in this block. Marker = the chain length right
            // after the push, so a later reorg rolling the chain back below it pops the
            // confirmation.
            self.observe_covenant(self.chain.len(), hdr.daa_score, &accepted);

            let mut s = self.stats.lock().unwrap();
            s.blocks_processed += 1;
            s.activity_executed = self.expected_counter as u64;
        }
    }

    /// If the pending covenant tx is accepted in this block, records it as confirmed at `marker`
    /// (the chain length after the block) and clears the pending slot. A settlement reaching here
    /// passed the L1 script engine (anchor + state chaining), so acceptance is itself the proof.
    fn observe_covenant(&mut self, marker: usize, daa_score: u64, accepted: &[Transaction]) {
        let Some(pending) = &self.pending else { return };
        if !accepted.iter().any(|tx| tx.id() == pending.txid) {
            return;
        }
        let mut covenant = pending.next.clone();
        covenant.daa_score = daa_score;
        let is_bootstrap = pending.is_bootstrap;
        self.confirmed.push(ConfirmedCovenant { marker, covenant });
        self.pending = None;
        self.reissues_since_progress = 0;
        if is_bootstrap {
            // Real-proof mode: now that the covenant id is live, rebuild the proving stack to bind
            // it. Deferred to the next `produce` so we don't tear down the scheduler
            // mid-`catch_up`.
            if self.real_e2e {
                self.init_proving_pending = true;
            }
        } else {
            self.stats.lock().unwrap().settlements_accepted += 1;
        }
    }

    /// Reconciles covenant state with a reorg that kept only `keep` chain blocks: pops every
    /// confirmation made in a rolled-back block, and abandons a pending tx whose basis (the
    /// covenant UTXO it spends) was popped; that tx can never land, so the next block
    /// re-issues from the new live tip.
    fn rollback_covenant(&mut self, keep: usize) {
        while self.confirmed.last().is_some_and(|c| c.marker > keep) {
            self.confirmed.pop();
        }
        if let Some(pending) = &self.pending {
            let live_tip = self.confirmed.last().map(|c| c.covenant.outpoint);
            if pending.basis_outpoint != live_tip {
                self.pending = None;
            }
        }
    }

    /// Builds this block's transactions: any covenant bootstrap/settlement that is due, plus seeded
    /// lane activity. Async because the real-proof settlement path awaits the aggregate prover's
    /// next bundle; the sole [`futures::executor::block_on`] is in [`Self::produce`], the sync
    /// `Producer` trait boundary (the simulation's `Process` is synchronous).
    async fn produce_txs(&mut self, ctx: &ProduceCtx<'_>) -> Vec<Transaction> {
        let mut txs = self.issue_covenant(ctx).await;
        txs.extend(self.issue_activity(ctx));
        txs
    }

    /// Issues a covenant bootstrap or settlement when one is due. Returns at most one transaction.
    ///
    /// While a tx is pending the slot stays occupied (no double-spend of the basis). If a pending
    /// tx stays unconfirmed past [`REISSUE_DEADLINE`] its block was orphaned, so it is dropped
    /// and the next due tx is re-issued from the live confirmed tip; persistent re-issues
    /// without progress trip the liveness guard.
    async fn issue_covenant(&mut self, ctx: &ProduceCtx<'_>) -> Vec<Transaction> {
        if !self.settlements_enabled {
            return Vec::new();
        }

        if let Some(pending) = &self.pending {
            if ctx.block_index.saturating_sub(pending.issued_block_index) < REISSUE_DEADLINE {
                return Vec::new();
            }
            // The pending tx's block was orphaned; abandon it and re-issue below.
            self.pending = None;
            self.reissues_since_progress += 1;
            self.stats.lock().unwrap().reissues += 1;
            assert!(
                self.reissues_since_progress <= MAX_REISSUES_WITHOUT_PROGRESS,
                "covenant liveness failure: re-issued {} times with no settlement landing",
                self.reissues_since_progress,
            );
        }

        if self.confirmed.is_empty() {
            return self.bootstrap(ctx);
        }
        if self.real_e2e {
            // The proving stack is rebuilt one block after the bootstrap confirms; until then
            // there are no bundles to settle.
            if !self.proving_ready {
                return Vec::new();
            }
            return self.settle_real(ctx).await;
        }
        // Dev settlement: settle on a seeded cadence once there is a chain tip to anchor to.
        if self.chain.is_empty() || !ctx.block_index.is_multiple_of(self.settle_every) {
            return Vec::new();
        }
        self.settle(ctx)
    }

    /// Builds the covenant bootstrap transaction funded from the largest spendable coinbase, sizing
    /// the locked value to half that output (so any subsidy fits and storage mass stays modest).
    fn bootstrap(&mut self, ctx: &ProduceCtx<'_>) -> Vec<Transaction> {
        let Some((outpoint, entry)) = ctx.spendable.first() else { return Vec::new() };
        let value = entry.amount / 2;
        let state = EMPTY_HASH;
        let lane_tip = Hash::default();
        // The production redeem (terminates in `OpZkPrecompile`) is deployed only for a real-proof
        // run: proving-driven settlement with real (CUDA) receipts. Every other case deploys the
        // dev redeem (chain-anchored seq commit, no precompile): the inline dev settlements, and
        // proving-driven settlement under `RISC0_DEV_MODE` (stub receipts the precompile would
        // reject). Both branches share their construction with the production settler
        // (`bootstrap_redeem` / `dev_bootstrap_redeem`) so the first settlement's reconstructed
        // prev redeem matches this UTXO's SPK.
        let (redeem, spk) = if self.real_e2e && !dev_mode_enabled() {
            bootstrap_redeem(&self.backend, &self.lane_key)
        } else {
            dev_bootstrap_redeem(&self.lane_key)
        };

        let (tx, covenant_id) = build::covenant_bootstrap_transaction(
            &redeem,
            value,
            *outpoint,
            entry.clone(),
            ctx.keypair,
            ctx.params,
        );
        let txid = tx.id();
        self.pending = Some(PendingCovenant {
            txid,
            basis_outpoint: None,
            issued_block_index: ctx.block_index,
            is_bootstrap: true,
            next: CovenantState {
                covenant_id,
                state,
                lane_tip,
                outpoint: TransactionOutpoint::new(txid, 0),
                spk,
                value,
                daa_score: 0,
            },
        });
        vec![tx]
    }

    /// Builds a dev settlement spending the live confirmed covenant, anchored to the current chain
    /// tip, with a fresh deterministic state. Funds the fee from a spendable coinbase.
    fn settle(&mut self, ctx: &ProduceCtx<'_>) -> Vec<Transaction> {
        let Some(confirmed) = self.confirmed.last() else { return Vec::new() };
        let Some((fee_outpoint, fee_entry)) = ctx.spendable.first() else { return Vec::new() };
        let cov = confirmed.covenant.clone();

        let tip = self.chain.last().expect("chain tip");
        let block_prove_to = tip.hash;
        let claimed_seq_commit = tip.meta.seq_commit;
        let new_lane_tip = claimed_seq_commit;
        // Index the next state by the number of confirmed covenant states, so a re-issue of an
        // orphaned settlement reproduces the same state (the confirmed count is unchanged) and a
        // landed one advances it, keeping prev_state == the live tip's state.
        let new_state = state_for(self.confirmed.len() as u64);

        let settlement = Settlement::build_dev(&SettlementDevInput {
            covenant_id: cov.covenant_id,
            prev_state: &cov.state,
            prev_lane_tip: &cov.lane_tip,
            lane_key: &self.lane_key,
            new_state: &new_state,
            new_lane_tip: &new_lane_tip,
            block_prove_to,
            claimed_seq_commit,
            prev_outpoint: cov.outpoint,
            value: cov.value,
        });
        let continuation_spk = pay_to_script_hash_script(&settlement.next_redeem);

        let covenant_entry =
            UtxoEntry::new(cov.value, cov.spk.clone(), cov.daa_score, false, Some(cov.covenant_id));
        let (xonly, _) = ctx.keypair.x_only_public_key();
        let address = Address::new(
            Prefix::from(ctx.params.net.network_type()),
            Version::PubKey,
            &xonly.serialize(),
        );
        let tx = build::settlement_transaction(build::SettlementTx {
            settlement_tx: settlement.transaction,
            covenant_entry,
            covenant_compute_budget: DEV_COVENANT_BUDGET,
            fee_outpoint: *fee_outpoint,
            fee_entry: fee_entry.clone(),
            keypair: ctx.keypair,
            address: &address,
            params: ctx.params,
        });
        let txid = tx.id();

        self.pending = Some(PendingCovenant {
            txid,
            basis_outpoint: Some(cov.outpoint),
            issued_block_index: ctx.block_index,
            is_bootstrap: false,
            next: CovenantState {
                covenant_id: cov.covenant_id,
                state: new_state,
                lane_tip: new_lane_tip,
                outpoint: TransactionOutpoint::new(txid, 0),
                spk: continuation_spk,
                value: cov.value,
                daa_score: 0,
            },
        });
        self.stats.lock().unwrap().settlements_issued += 1;
        vec![tx]
    }

    /// Settles the next proven bundle the in-process aggregate prover publishes, driven by the
    /// bundle's settlement artifact. Returns at most one transaction.
    ///
    /// Blocks for bundle handles only once a full `bundle_size` worth of batches has been submitted
    /// to the prover, giving the back-pressure that keeps the simulated clock (wall-time-free
    /// logical ticks) from outpacing the detached prover thread. Every submitted batch is accounted
    /// by exactly one bundle's `batches`, so the blocking pop never deadlocks: an outstanding batch
    /// guarantees a forthcoming bundle. The handle is published before its proof exists, so its
    /// `batches` is reconciled on pop and its artifact awaited after. No-op bundles carry no
    /// artifact and are skipped; a state-advancing one is built into a settlement that spends the
    /// live covenant and chains to the journal's `new_state` / `new_lane_tip`. The on-chain
    /// `OpZkPrecompile` validates the receipt, so acceptance (observed in
    /// [`Self::observe_covenant`]) proves the real proof verified.
    ///
    /// Reached only when no covenant tx is pending (see [`Self::issue_covenant`]), so settlements
    /// are serialized: at most one in flight, the next built after the previous one confirms.
    async fn settle_real(&mut self, ctx: &ProduceCtx<'_>) -> Vec<Transaction> {
        // Wait until a full bundle's worth of batches has been submitted before pulling
        // settlements, mirroring the previous bundle cadence and bounding how far the miner
        // runs ahead of proving.
        if self.outstanding_batches < self.bundle_size {
            return Vec::new();
        }
        // A fee output funds the settlement; if none is free this block, retry next block without
        // consuming a bundle.
        if ctx.spendable.is_empty() {
            return Vec::new();
        }

        // Clone the queue handle out of `self` so the loop can await it without holding a borrow
        // across the later `&mut self` mutations in `build_settlement_tx`.
        let queue = self.settlement_queue.clone().expect("aggregate prover queue");
        loop {
            // An outstanding batch guarantees the prover will publish a covering bundle, so this
            // await cannot hang; it is exactly the proving back-pressure. The `batches` count is
            // immediate (set before proving), so reconcile it on pop before awaiting the artifact.
            let bundle = queue.wait_and_pop().await;
            self.outstanding_batches = self.outstanding_batches.saturating_sub(bundle.batches());
            bundle.wait_artifact_published().await;
            if let Some(artifact) = bundle.artifact() {
                return self.build_settlement_tx(ctx, &artifact);
            }
            // No-op bundle: nothing to settle. Stop once the backlog drains below a full bundle.
            if self.outstanding_batches < self.bundle_size {
                return Vec::new();
            }
        }
    }

    /// Builds a real-proof settlement transaction from a proved bundle's artifact: spends the live
    /// confirmed covenant, chains to the artifact's `new_state` / `new_lane_tip`, funds the fee
    /// from a spendable coinbase, and records it pending. Returns the single settlement
    /// transaction.
    fn build_settlement_tx(
        &mut self,
        ctx: &ProduceCtx<'_>,
        artifact: &SettlementArtifact<Receipt>,
    ) -> Vec<Transaction> {
        let (fee_outpoint, fee_entry) = ctx.spendable.first().expect("fee output");
        let cov = self.confirmed.last().expect("covenant").covenant.clone();

        // Build the settlement and its covenant compute budget the same way the production settler
        // does; the sim only differs in how it funds the fee and submits the tx (mined by its
        // miner, not the wallet's wRPC client). Under `RISC0_DEV_MODE` the receipt is a stub the
        // production `OpZkPrecompile` would reject, so settle against the dev redeem (shared
        // `build_dev_settlement`); a real (CUDA) run settles the production receipt.
        let built = if dev_mode_enabled() {
            build_dev_settlement(&self.lane_key, &cov, artifact)
        } else {
            build_settlement(&self.backend, &self.lane_key, &cov, artifact)
        };

        let covenant_entry =
            UtxoEntry::new(cov.value, cov.spk.clone(), cov.daa_score, false, Some(cov.covenant_id));
        let (xonly, _) = ctx.keypair.x_only_public_key();
        let address = Address::new(
            Prefix::from(ctx.params.net.network_type()),
            Version::PubKey,
            &xonly.serialize(),
        );
        let tx = build::settlement_transaction(build::SettlementTx {
            settlement_tx: built.transaction,
            covenant_entry,
            covenant_compute_budget: built.compute_budget,
            fee_outpoint: *fee_outpoint,
            fee_entry: fee_entry.clone(),
            keypair: ctx.keypair,
            address: &address,
            params: ctx.params,
        });
        let txid = tx.id();

        self.pending = Some(PendingCovenant {
            txid,
            basis_outpoint: Some(cov.outpoint),
            issued_block_index: ctx.block_index,
            is_bootstrap: false,
            // daa_score stamped on acceptance in `observe_covenant`.
            next: built.advance.apply(txid, 0),
        });
        self.stats.lock().unwrap().settlements_issued += 1;
        vec![tx]
    }

    /// Issues a seeded number of lane-activity transactions, each writing the tracked resource,
    /// from the miner's spendable coinbase. Skips the first spendable output, which covenant
    /// txs use.
    fn issue_activity(&mut self, ctx: &ProduceCtx<'_>) -> Vec<Transaction> {
        // Real-proof mode issues no activity until its proving stack is live (post-bootstrap), so
        // every proved batch binds the real covenant id.
        if self.real_e2e && !self.proving_ready {
            return Vec::new();
        }
        // Leave the first spendable output for covenant funding (bootstrap / settlement fee).
        let available = ctx.spendable.len().saturating_sub(1);
        let n = (self.rng.gen_range(0..=self.activity_per_block) as usize).min(available);
        if n == 0 {
            return Vec::new();
        }

        let (xonly, _) = ctx.keypair.x_only_public_key();
        let address = Address::new(
            Prefix::from(ctx.params.net.network_type()),
            Version::PubKey,
            &xonly.serialize(),
        );
        let payload = encode_activity_payload(&[AccessMetadata::write(self.tracked)], &[1, 2, 3]);

        ctx.spendable[1..=n]
            .iter()
            .map(|(outpoint, entry)| {
                build::activity_transaction(build::ActivityTx {
                    payload: payload.clone(),
                    outpoint: *outpoint,
                    entry: entry.clone(),
                    keypair: ctx.keypair,
                    address: &address,
                    subnetwork_id: self.lane_subnet,
                    tx_version: TX_VERSION_TOCCATA,
                    params: ctx.params,
                })
            })
            .collect()
    }
}

impl Drop for L2Driver {
    fn drop(&mut self) {
        // The simulation drops the scheduler without calling `Scheduler::shutdown`, so signal the
        // batch-prover worker directly (via the shared pipeline) to flush and exit; otherwise it
        // would loop forever holding this driver's store. No-op for the execution-only pipeline.
        self.exec.proc_handle.on_shutdown();
    }
}

impl Producer for L2Driver {
    fn produce(&mut self, ctx: ProduceCtx<'_>) -> Vec<Transaction> {
        // Rebuild the proving stack with the live covenant id the block after the bootstrap
        // confirmed (real-proof mode); see [`Self::init_proving`].
        if self.init_proving_pending {
            self.init_proving_pending = false;
            self.init_proving();
        }
        self.catch_up(ctx.consensus);
        if ctx.draining {
            // Winding down past the block target: issue no new work, and once the final settlement
            // has confirmed (observed by the `catch_up` above) open the latch so the miner stops
            // mining drain blocks. `pending` only clears once here, since no new work is issued.
            if self.pending.is_none() {
                self.drained.open();
            }
            return Vec::new();
        }
        // The simulation's `Process` (and so `Producer`) is synchronous; the real-proof settlement
        // path is async (it awaits the prover's next bundle). Bridge the seam here with a single
        // `block_on`, mirroring how the miner awaits `validate_and_insert_block`.
        futures::executor::block_on(self.produce_txs(&ctx))
    }
}

/// A deterministic, distinct 32-byte L2 state for settlement number `n` (dev settlements don't
/// verify the state against execution, but it must chain: settlement n+1's prev == settlement n's).
fn state_for(n: u64) -> [u8; 32] {
    let mut s = [0u8; 32];
    s[..8].copy_from_slice(&n.to_le_bytes());
    s
}

/// Baseline metadata for the seed (genesis-side) block: only the header-derived fields, no parent.
fn base_meta(
    hash: Hash,
    hdr: &kaspa_consensus_core::header::Header,
    lane_key: Hash,
) -> ChainBlockMetadata {
    ChainBlockMetadata {
        hash,
        blue_score: hdr.blue_score,
        daa_score: hdr.daa_score,
        timestamp: hdr.timestamp,
        seq_commit: hdr.accepted_id_merkle_root,
        lane_key,
        ..Default::default()
    }
}

/// Metadata for a scheduled chain block, threading the parent's fields into the `prev_*` slots.
fn child_meta(
    hash: Hash,
    hdr: &kaspa_consensus_core::header::Header,
    parent: &ChainBlockMetadata,
    lane_key: Hash,
) -> ChainBlockMetadata {
    ChainBlockMetadata {
        hash,
        blue_score: hdr.blue_score,
        daa_score: hdr.daa_score,
        timestamp: hdr.timestamp,
        seq_commit: hdr.accepted_id_merkle_root,
        prev_seq_commit: parent.seq_commit,
        prev_timestamp: parent.timestamp,
        lane_key,
        prev_lane_tip: parent.lane_tip,
        prev_lane_blue_score: parent.lane_blue_score,
        lane_blue_score: parent.lane_blue_score,
        lane_tip: parent.lane_tip,
        last_settlement: parent.last_settlement,
        ..Default::default()
    }
}
