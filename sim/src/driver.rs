//! The L2 driver: a [`Producer`] that runs the full L2 stack against the simulated chain.
//!
//! Attached to one miner, it (1) follows that node's selected chain, scheduling each block's
//! lane-activity transactions through the zk `Vm` and handling reorgs via `rollback_to`; (2) issues
//! new seeded lane-activity transactions funded from the miner's coinbase; (3) optionally
//! bootstraps a covenant and periodically settles it (dev settlements, validated by the sim's real
//! script engine); and (4) asserts invariants every block. The strongest is that the decoded L2
//! counter equals the number of lane-activity transactions executed on the current selected chain;
//! a failing invariant panics with the block hash, so a fixed seed pinpoints the bug.

use std::{
    collections::VecDeque,
    num::NonZeroUsize,
    sync::{Arc, Mutex, Weak},
};

use kaspa_addresses::{Address, Prefix, Version};
use kaspa_consensus::consensus::Consensus;
use kaspa_consensus_core::{
    api::ConsensusApi,
    constants::TX_VERSION_TOCCATA,
    mass::units::ComputeBudget,
    subnets::SubnetworkId,
    tx::{
        ScriptPublicKey, Transaction, TransactionOutpoint, TransactionQueryResult, TransactionType,
        UtxoEntry,
    },
};
use kaspa_hashes::Hash;
use kaspa_seq_commit::hashing::lane_key;
use kaspa_txscript::standard::pay_to_script_hash_script;
use rand::{Rng, SeedableRng, rngs::StdRng};
use tempfile::TempDir;
use vprogs_core_codec::Reader;
use vprogs_core_smt::EMPTY_HASH;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId, SchedulerTransaction};
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_l1_wallet::{build, encode_activity_payload};
use vprogs_scheduling_scheduler::{Processor, ScheduledBatch, Scheduler};
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_abi::batch_processor::StateTransition;
// `Backend as _` brings the batch-prover `Backend` trait into scope for
// `Backend::journal_bytes`.
use vprogs_zk_backend_risc0_api::{Backend, OwnedSuccinctWitness, ProofType};
use vprogs_zk_backend_risc0_covenant::{
    CommonPins, DEFAULT_PERMISSION_OUTPUT_VALUE, RedeemPins, Settlement, SettlementDevInput,
    SettlementInput, SuccinctPins, build_dev_redeem_script, build_redeem_script,
    dev_redeem_script_len, redeem_script_len,
};
use vprogs_zk_backend_risc0_test_suite::{
    batch_processor_elf, build_scheduler, read_resource_u32, transaction_processor_elf,
};
use vprogs_zk_batch_prover::{Backend as _, BatchProverConfig};
use vprogs_zk_vm::{ProvingPipeline, Vm};

use crate::{
    l2_miner::{ProduceCtx, Producer},
    lane_source::ConsensusLaneSource,
};

type Store = RocksDbStore;
type V = Vm<Backend, Store>;

/// Compute budget for the dev covenant input (dev redeem has no precompile; 100 covers it).
const DEV_COVENANT_BUDGET: ComputeBudget = ComputeBudget(100);

/// Compute budget for a real-proof covenant input. `OpZkPrecompile` for the succinct branch burns
/// ~2500 units; ship headroom (mirrors `settlement_l1_e2e`).
const REAL_COVENANT_BUDGET: ComputeBudget = ComputeBudget(10_000);

/// Construction parameters for the driver.
pub struct L2Config {
    /// Lane id; selects the lane subnetwork, lane key, and the tracked resource.
    pub lane_id: u32,
    /// Seed for the activity / settlement RNG.
    pub seed: u64,
    /// Max activity transactions issued per block (the actual count is seeded `0..=this`).
    pub activity_per_block: u64,
    /// Bootstrap a covenant and settle it. When false the driver only does activity + execution.
    pub enable_settlements: bool,
    /// Issue a settlement roughly every this-many blocks once the covenant is active (ignored when
    /// settlements are disabled).
    pub settle_every: u64,
    /// Drive the real batch prover (`ProvingPipeline::batch`) off the simulation's consensus
    /// instead of running execution-only. With the crate's `cuda` feature the proofs are real
    /// GPU proofs; without it (or under `RISC0_DEV_MODE=1`) the proving machinery still runs
    /// end to end with the CPU/dev executor, which is what makes the wiring testable without a
    /// GPU.
    pub enable_proving: bool,
    /// Batches bundled per proof when `enable_proving` is set (clamped to at least 1).
    pub bundle_size: usize,
}

/// Running totals the driver maintains, readable after a run for end-of-test assertions.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct DriverStats {
    /// Selected-chain blocks scheduled (net of rollbacks).
    pub blocks_processed: u64,
    /// Lane-activity transactions executed on the current selected chain.
    pub activity_executed: u64,
    /// Reorgs observed (non-empty removed sets).
    pub reorgs: u64,
    /// Deepest reorg (blocks rolled back in one event).
    pub max_reorg_depth: u64,
    /// Covenant settlement transactions issued (each emission, including a re-issue of an orphaned
    /// settlement, counts).
    pub settlements_issued: u64,
    /// Covenant settlements that landed and chained successfully.
    pub settlements_accepted: u64,
    /// Covenant txs re-issued after their block was orphaned by a reorg (0 on a clean chain).
    pub reissues: u64,
}

/// One selected-chain block the driver has scheduled, kept so reorgs can roll back exactly.
struct BlockRec {
    hash: Hash,
    meta: ChainBlockMetadata,
    lane_tx_count: u32,
}

/// The on-chain covenant: its identity and current committed state, plus the UTXO that carries it.
#[derive(Clone)]
struct Covenant {
    covenant_id: Hash,
    state: [u8; 32],
    lane_tip: Hash,
    outpoint: TransactionOutpoint,
    spk: ScriptPublicKey,
    value: u64,
    daa_score: u64,
}

/// A covenant state seen accepted on the current selected chain, tagged with the chain length right
/// after the block that confirmed it. A reorg that rolls the chain back below `marker` pops this
/// entry, so the covenant history always matches the live chain. `confirmed[0]` is the bootstrap.
struct ConfirmedCovenant {
    marker: usize,
    covenant: Covenant,
}

/// A covenant transaction issued into a block but not yet seen accepted on the selected chain.
///
/// If its block loses the chain race (orphaned), the tx never lands; after [`REISSUE_DEADLINE`]
/// mined blocks the driver re-issues from the live confirmed tip. `basis_outpoint` is the covenant
/// UTXO this tx spends (None for the coinbase-funded bootstrap) — when a reorg pops that basis off
/// `confirmed`, the pending tx is abandoned immediately rather than waiting for the deadline.
struct PendingCovenant {
    txid: Hash,
    basis_outpoint: Option<TransactionOutpoint>,
    issued_block_index: u64,
    next: Covenant,
    is_bootstrap: bool,
}

/// Mined blocks a pending covenant tx may stay unconfirmed before the driver assumes its block was
/// orphaned and re-issues. Set well above the expected reorg depth so a re-issue only fires once
/// the orphaned branch is too deep to return (which would otherwise double-spend the basis).
const REISSUE_DEADLINE: u64 = 30;

/// If a covenant tx is re-issued this many times without any settlement landing, the covenant can
/// never make progress — a liveness failure. Panics with the seed so it is reproducible.
const MAX_REISSUES_WITHOUT_PROGRESS: u64 = 200;

/// The L2 execution stack: the zk `Vm`-backed scheduler over a temp-backed store. Held as a unit
/// so the real-proof mode can rebuild it once the covenant id is known (the batch prover binds the
/// covenant id into every journal, and it isn't known until the bootstrap is mined).
struct Exec {
    scheduler: Scheduler<Store, V>,
    store: Store,
    _db: TempDir,
    /// A clone of the processor (shares the proving pipeline) kept only so [`Drop`] can signal the
    /// batch-prover worker to shut down — the simulation drops the scheduler without calling
    /// `Scheduler::shutdown`, so the worker would otherwise loop forever. No-op when proving is
    /// off.
    proc_handle: V,
}

/// The L2 driver. Owns the execution stack + store and tracks the selected chain it has scheduled.
pub struct L2Driver {
    lane_subnet: SubnetworkId,
    lane_key: Hash,
    tracked: ResourceId,
    activity_per_block: u64,
    settle_every: u64,
    bundle_size: usize,
    rng: StdRng,

    backend: Backend,
    exec: Exec,
    /// Weak handle to the node's consensus, used to (re)build the batch prover's lane source after
    /// bootstrap. Weak so it never extends the consensus lifetime (see [`ConsensusLaneSource`]).
    consensus: Weak<Consensus>,

    seeded: bool,
    seed_meta: ChainBlockMetadata,
    chain: Vec<BlockRec>,
    expected_counter: u32,

    settlements_enabled: bool,
    /// Real-proof end-to-end mode: prove each bundle and settle it with a production
    /// `Settlement::build` (real receipt → `OpZkPrecompile`), instead of dev settlements. Implied
    /// by `enable_proving && enable_settlements`. Single-miner only (a reorg can orphan a block
    /// whose batch the async worker is proving).
    real_e2e: bool,
    /// In `real_e2e`, false until the proving stack has been rebuilt with the live covenant id
    /// (after the bootstrap confirms); gates activity + settlement so nothing is proved against
    /// the placeholder covenant id. Always true in the other modes.
    proving_ready: bool,
    /// Set when the bootstrap confirms in `real_e2e`; the next `produce` rebuilds the proving
    /// stack.
    init_proving_pending: bool,
    /// Committed batches awaiting bundle proof + settlement, in scheduling order (real_e2e only).
    /// The front `bundle_size` form the prover's next bundle; once their receipt publishes the
    /// driver settles it. Empty in the other modes.
    unproved: VecDeque<ScheduledBatch<Store, V>>,

    /// Covenant states confirmed on the live chain (`confirmed[0]` = bootstrap); empty until the
    /// bootstrap lands. Popped on reorg, so the tip is always the live covenant.
    confirmed: Vec<ConfirmedCovenant>,
    /// The in-flight covenant tx awaiting acceptance, if any.
    pending: Option<PendingCovenant>,
    /// Re-issues since the last settlement landed; reset on progress, bounds liveness.
    reissues_since_progress: u64,

    stats: Arc<Mutex<DriverStats>>,
}

/// Builds a fresh execution stack (temp store + zk `Vm` scheduler). When `proving` is set the `Vm`
/// drives the real batch prover, reading lane proofs from `consensus` and binding `covenant_id`
/// into every journal; otherwise it is execution-only.
fn build_exec(
    backend: &Backend,
    lane: Hash,
    bundle_size: usize,
    proving: bool,
    covenant_id: Option<Hash>,
    consensus: Weak<Consensus>,
) -> Exec {
    let db = tempfile::tempdir().expect("temp db dir");
    let store = RocksDbStore::open(db.path().join("l2"));
    let pipeline = if proving {
        ProvingPipeline::batch(
            backend.clone(),
            store.clone(),
            ConsensusLaneSource::from_weak(consensus),
            BatchProverConfig {
                bundle_size: NonZeroUsize::new(bundle_size.max(1)).expect("nonzero"),
                lane_key: lane,
                covenant_id,
            },
        )
    } else {
        ProvingPipeline::None
    };
    let vm = Vm::new(backend.clone(), pipeline);
    let proc_handle = vm.clone();
    let scheduler = build_scheduler(vm, store.clone());
    Exec { scheduler, store, _db: db, proc_handle }
}

impl L2Driver {
    /// Builds a driver with a fresh temp-backed store and a zk `Vm`. When `config.enable_proving`
    /// is set the `Vm` drives the real batch prover, reading lane proofs from `consensus` (the
    /// node this driver's miner runs); otherwise it is execution-only. Returns the driver and a
    /// shared stats handle the test can read after the run.
    pub fn new(config: L2Config, consensus: &Arc<Consensus>) -> (Self, Arc<Mutex<DriverStats>>) {
        let backend =
            Backend::new(&transaction_processor_elf(), &batch_processor_elf(), ProofType::Succinct);

        let lane_subnet = SubnetworkId::from_namespace(config.lane_id.to_be_bytes());
        let lane = lane_key(lane_subnet.as_bytes());
        let weak = Arc::downgrade(consensus);

        // Real-proof end-to-end: prove and settle from real receipts. Its proving stack is built
        // lazily (after bootstrap, with the real covenant id), so start execution-only. The
        // prove-only mode (proving without settlements) builds its batch pipeline now, binding the
        // zero placeholder covenant id since no on-chain settlement consumes those receipts.
        let real_e2e = config.enable_proving && config.enable_settlements;
        let prove_only = config.enable_proving && !config.enable_settlements;
        let exec = build_exec(&backend, lane, config.bundle_size, prove_only, None, weak.clone());

        let stats = Arc::new(Mutex::new(DriverStats::default()));
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
            unproved: VecDeque::new(),
            confirmed: Vec::new(),
            pending: None,
            reissues_since_progress: 0,
            stats: stats.clone(),
        };
        (driver, stats)
    }

    /// Rebuilds the execution stack with the real batch prover bound to the live covenant id, over
    /// a fresh store. Called once, right after the bootstrap confirms: the proven bundles' journals
    /// then commit the real covenant id (the on-chain script rejects the zero placeholder). The new
    /// store starts at the empty SMT, matching the bootstrap's `EMPTY_HASH` state, so the first
    /// proved bundle chains from the bootstrap. Execution/cursor state resets to follow the chain
    /// fresh from the current sink (only post-bootstrap activity is proved + settled).
    fn init_proving(&mut self) {
        let covenant_id = self.confirmed[0].covenant.covenant_id;
        self.exec = build_exec(
            &self.backend,
            self.lane_key,
            self.bundle_size,
            true,
            Some(covenant_id),
            self.consensus.clone(),
        );
        self.proving_ready = true;
        self.seeded = false;
        self.seed_meta = ChainBlockMetadata::default();
        self.chain.clear();
        self.expected_counter = 0;
        self.unproved.clear();
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
            // async prover may already be bundling, desyncing `unproved` from the prover's stream
            // (which independently drops rolled-back batches). Fail loudly rather than settle a
            // bundle proved against a dead chain. See the test / TODO for the framework-side fix
            // (cancellation in `process_bundle`).
            assert!(
                !(self.real_e2e && self.proving_ready),
                "real-proof settlement requires a single miner; got a reorg",
            );
            let depth = path.removed.len() as u64;
            let keep = self.chain.len().saturating_sub(path.removed.len());
            for b in self.chain.drain(keep..) {
                self.expected_counter -= b.lane_tx_count;
            }
            self.exec.scheduler.rollback_to(keep as u64).expect("rollback");
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
                if let Ok(proof) = c.get_seq_commit_lane_proof(hash, self.lane_key) {
                    if let Some(tip) = proof.lane_tip {
                        meta.lane_tip = tip;
                    }
                    if let Some(bs) = proof.lane_blue_score {
                        meta.lane_blue_score = bs;
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
            batch.wait_committed_blocking();
            // In real-proof mode hold the committed batch so its bundle receipt can drive a
            // settlement once the prover publishes it; other modes drop it (no settlement consumes
            // the receipt).
            if self.real_e2e && self.proving_ready {
                self.unproved.push_back(batch);
            }
            self.expected_counter += count;
            self.chain.push(BlockRec { hash, meta, lane_tx_count: count });

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
    /// covenant UTXO it spends) was popped — that tx can never land, so the next block
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
    /// lane activity.
    fn produce_txs(&mut self, ctx: &ProduceCtx<'_>) -> Vec<Transaction> {
        let mut txs = self.issue_covenant(ctx);
        txs.extend(self.issue_activity(ctx));
        txs
    }

    /// Issues a covenant bootstrap or settlement when one is due. Returns at most one transaction.
    ///
    /// While a tx is pending the slot stays occupied (no double-spend of the basis). If a pending
    /// tx stays unconfirmed past [`REISSUE_DEADLINE`] its block was orphaned, so it is dropped
    /// and the next due tx is re-issued from the live confirmed tip; persistent re-issues
    /// without progress trip the liveness guard.
    fn issue_covenant(&mut self, ctx: &ProduceCtx<'_>) -> Vec<Transaction> {
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
            return self.settle_real(ctx);
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
        // Real-proof mode deploys the production redeem script (terminates in `OpZkPrecompile`) so
        // the first settlement's reconstructed prev redeem matches this UTXO's SPK; dev mode uses
        // the dev redeem (chain-anchored, no precompile).
        let redeem = if self.real_e2e {
            let pins = self.redeem_pins();
            let redeem_len = redeem_script_len(&state, &pins);
            build_redeem_script(&state, &lane_tip, redeem_len, &pins)
        } else {
            let redeem_len = dev_redeem_script_len(&state, &self.lane_key);
            build_dev_redeem_script(&state, &lane_tip, &self.lane_key, redeem_len)
        };
        let spk = pay_to_script_hash_script(&redeem);

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
            next: Covenant {
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
        // landed one advances it — keeping prev_state == the live tip's state.
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
            next: Covenant {
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

    /// The production redeem pins for this covenant: the batch + transaction guest image ids, the
    /// lane key, and the default permission-output value. Stable across the run (image ids are
    /// fixed), so bootstrap and every settlement share them.
    fn redeem_pins(&self) -> RedeemPins<'_> {
        RedeemPins::Succinct(SuccinctPins {
            common: CommonPins {
                program_id: self.backend.batch_image_id(),
                tx_image_id: self.backend.transaction_image_id(),
                lane_key: &self.lane_key,
                permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
            },
        })
    }

    /// Settles the next proven bundle with a production `Settlement::build`, driven by the bundle's
    /// real receipt. Returns at most one transaction.
    ///
    /// Consumes the front `bundle_size` committed batches once their bundle receipt has published,
    /// parses the bundle journal, and — if the bundle advanced the L2 state — builds a settlement
    /// that spends the live covenant and chains to the journal's `new_state` / `new_lane_tip`. The
    /// on-chain `OpZkPrecompile` validates the receipt, so acceptance (observed in
    /// [`Self::observe_covenant`]) proves the real proof verified against the covenant. A no-op
    /// bundle (state unchanged) is dropped without settling.
    ///
    /// Reached only when no covenant tx is pending (see [`Self::issue_covenant`]), so settlements
    /// are serialized: at most one in flight, the next built after the previous one confirms. The
    /// front `bundle_size` of `unproved` is exactly the prover's next bundle (both consume batches
    /// in scheduling order), so the receipt on the last batch is this bundle's.
    fn settle_real(&mut self, ctx: &ProduceCtx<'_>) -> Vec<Transaction> {
        // A full bundle's worth of batches must be queued and its receipt published. The prover
        // publishes the bundle receipt to every batch in the bundle, so the last one carries it.
        if self.unproved.len() < self.bundle_size
            || !self.unproved[self.bundle_size - 1].artifact_published()
        {
            return Vec::new();
        }
        let bundle: Vec<_> =
            (0..self.bundle_size).map(|_| self.unproved.pop_front().unwrap()).collect();
        let last = bundle.last().unwrap();
        let block_prove_to = last.checkpoint().metadata().hash;
        let claimed_seq_commit = last.checkpoint().metadata().seq_commit;
        let receipt = (*last.artifact()).clone();

        let journal = Backend::journal_bytes(&receipt);
        let parsed =
            (&mut &journal[..]).array_as::<StateTransition>("state_transition").expect("journal");

        // A no-op bundle (no lane activity landed in its blocks) leaves the state unchanged; there
        // is nothing to settle and the next bundle still chains from the live covenant.
        if parsed.new_state == parsed.prev_state {
            return Vec::new();
        }

        let Some((fee_outpoint, fee_entry)) = ctx.spendable.first() else { return Vec::new() };
        let cov = self.confirmed.last().expect("covenant").covenant.clone();
        assert_eq!(
            parsed.prev_state, cov.state,
            "settlement prev_state must chain from the live covenant state",
        );
        assert_eq!(
            parsed.prev_lane_tip, cov.lane_tip,
            "settlement prev_lane_tip must match the spent covenant's redeem prefix",
        );
        assert_eq!(
            parsed.new_seq_commit, claimed_seq_commit,
            "journal new_seq_commit must equal block_prove_to's seq_commit",
        );
        assert_eq!(
            Hash::from_bytes(parsed.covenant_id),
            cov.covenant_id,
            "journal covenant_id must match the live covenant (prover seeded with wrong id)",
        );

        let new_state = parsed.new_state;
        let new_lane_tip = parsed.new_lane_tip;
        let owned_witness = OwnedSuccinctWitness::from_receipt(&receipt);
        let settlement = Settlement::build(&SettlementInput {
            covenant_id: cov.covenant_id,
            pins: self.redeem_pins(),
            prev_state: &parsed.prev_state,
            prev_lane_tip: &parsed.prev_lane_tip,
            new_state: &new_state,
            new_lane_tip: &new_lane_tip,
            block_prove_to,
            prev_outpoint: cov.outpoint,
            value: cov.value,
            witness: owned_witness.as_witness(),
            permission_spk_hash: &parsed.permission_spk_hash,
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
            covenant_compute_budget: REAL_COVENANT_BUDGET,
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
            next: Covenant {
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
        self.produce_txs(&ctx)
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
