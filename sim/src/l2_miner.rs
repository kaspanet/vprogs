//! A simulation miner whose block transactions come from a [`Producer`].
//!
//! Mechanically this mirrors simpa's `Miner` (mine on a Poisson timer, broadcast, insert received
//! blocks, track its own matured coinbase outpoints). The one difference is tx generation: instead
//! of simpa's fixed coinbase-splitting, each block delegates to a [`Producer`] that returns
//! already-signed transactions — letting the L2 driver issue covenant, activity, and settlement txs
//! funded from this miner's coinbase. simpa's `Miner` is reused as-is for the filler miners; this
//! type exists only because a `LaneProducer` cannot inject arbitrary txs.

use std::{cmp::max, iter::once, sync::Arc};

use indexmap::IndexSet;
use kaspa_consensus::{
    consensus::Consensus, model::stores::virtual_state::VirtualStateStoreReader,
};
use kaspa_consensus_core::{
    api::ConsensusApi,
    block::{Block, TemplateBuildMode, TemplateTransactionSelector},
    coinbase::MinerData,
    config::params::Params,
    tx::{ScriptPublicKey, ScriptVec, Transaction, TransactionId, TransactionOutpoint, UtxoEntry},
    utxo::utxo_view::UtxoView,
};
use kaspa_utils::sim::{Environment, Process, Resumption, Suspension};
use rand::rngs::StdRng;
use rand_distr::{Distribution, Exp};
use secp256k1::Keypair;

/// What a [`Producer`] receives for one block. Spendable outpoints are snapshotted and the virtual
/// read guard released before this is built, so the producer may freely call read-only
/// [`ConsensusApi`] methods (each takes its own session).
pub struct ProduceCtx<'a> {
    /// This miner's id.
    pub miner_id: u64,
    /// Logical simulation time of this block (ms).
    pub sim_time: u64,
    /// This miner's 0-based block counter.
    pub block_index: u64,
    /// The miner's key; sign funded inputs with it.
    pub keypair: Keypair,
    /// Matured outpoints (with entries) available to spend, in insertion order.
    pub spendable: Vec<(TransactionOutpoint, UtxoEntry)>,
    /// Read-only consensus access for deriving chain/lane state.
    pub consensus: &'a dyn ConsensusApi,
    /// Consensus params.
    pub params: &'a Params,
}

/// Produces the transactions a miner includes in one block (already signed, mass committed).
pub trait Producer: Send {
    /// Returns the transactions for this block. They are inserted as-is.
    fn produce(&mut self, ctx: ProduceCtx<'_>) -> Vec<Transaction>;
}

/// A miner whose block contents come from a [`Producer`].
pub struct L2Miner {
    id: u64,
    consensus: Arc<Consensus>,
    params: Params,
    miner_data: MinerData,
    keypair: Keypair,
    possible_unspent_outpoints: IndexSet<TransactionOutpoint>,
    dist: Exp<f64>,
    rng: StdRng,
    num_blocks: u64,
    sim_time: u64,
    target_blocks: Option<u64>,
    max_spendable_snapshot: usize,
    max_cached_outpoints: usize,
    producer: Box<dyn Producer>,
}

impl L2Miner {
    /// Builds an L2 miner. `bps * hashrate` sets the Poisson mining rate (match the filler miners).
    /// `max_spendable_snapshot` caps how many matured outpoints are handed to the producer per
    /// block.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: u64,
        bps: f64,
        hashrate: f64,
        keypair: Keypair,
        consensus: Arc<Consensus>,
        params: &Params,
        rng: StdRng,
        target_blocks: Option<u64>,
        max_spendable_snapshot: usize,
        producer: Box<dyn Producer>,
    ) -> Self {
        let (schnorr_public_key, _) = keypair.public_key().x_only_public_key();
        // Standard p2pk script: push-32, the x-only pubkey, OP_CHECKSIG. Matches simpa's miner so a
        // received block's change outputs to this key are recognized as spendable.
        let script =
            once(0x20).chain(schnorr_public_key.serialize()).chain(once(0xac)).collect::<Vec<u8>>();
        let miner_data =
            MinerData::new(ScriptPublicKey::new(0, ScriptVec::from_slice(&script)), Vec::new());
        Self {
            id,
            consensus,
            params: params.clone(),
            miner_data,
            keypair,
            possible_unspent_outpoints: IndexSet::new(),
            dist: Exp::new(bps * hashrate).unwrap(),
            rng,
            num_blocks: 0,
            sim_time: 0,
            target_blocks,
            max_spendable_snapshot,
            max_cached_outpoints: 10_000,
            producer,
        }
    }

    fn build_new_block(&mut self, timestamp: u64) -> Block {
        let txs = self.build_txs();
        let session = self.consensus.acquire_session();
        let mut block_template = self
            .consensus
            .build_block_template(self.miner_data.clone(), Box::new(OnetimeTxSelector::new(txs)), TemplateBuildMode::Standard)
            .expect("simulation txs are selected in sync with virtual state and are expected to be valid");
        drop(session);
        block_template.block.header.timestamp = timestamp;
        block_template.block.header.nonce = self.id;
        block_template.block.header.finalize();
        block_template.block.to_immutable()
    }

    fn build_txs(&mut self) -> Vec<Transaction> {
        // Snapshot matured spendable outpoints, then release the virtual read guard before the
        // producer runs (it reads the consensus through its own sessions).
        let spendable: Vec<(TransactionOutpoint, UtxoEntry)> = {
            let virtual_read = self.consensus.virtual_stores.read();
            let virtual_state = virtual_read.state.get().unwrap();
            let virtual_utxo_view = &virtual_read.utxo_set;
            let daa_score = virtual_state.daa_score;
            self.possible_unspent_outpoints
                .iter()
                .filter_map(|&outpoint| {
                    self.get_spendable_entry(virtual_utxo_view, outpoint, daa_score)
                        .map(|entry| (outpoint, entry))
                })
                .take(self.max_spendable_snapshot)
                .collect()
        };

        let txs = self.producer.produce(ProduceCtx {
            miner_id: self.id,
            sim_time: self.sim_time,
            block_index: self.num_blocks,
            keypair: self.keypair,
            spendable,
            consensus: self.consensus.as_ref(),
            params: &self.params,
        });

        for outpoint in txs.iter().flat_map(|t| t.inputs.iter().map(|i| i.previous_outpoint)) {
            self.possible_unspent_outpoints.swap_remove(&outpoint);
        }
        txs
    }

    fn get_spendable_entry(
        &self,
        utxo_view: &impl UtxoView,
        outpoint: TransactionOutpoint,
        virtual_daa_score: u64,
    ) -> Option<UtxoEntry> {
        let entry = utxo_view.get(&outpoint)?;
        if entry.amount < 2
            || (entry.is_coinbase
                && (virtual_daa_score as i64 - entry.block_daa_score as i64)
                    <= self.params.coinbase_maturity() as i64)
        {
            return None;
        }
        Some(entry)
    }

    fn mine(&mut self, env: &mut Environment<Block>) -> Suspension {
        let block = self.build_new_block(env.now());
        env.broadcast(self.id, block);
        self.sample_mining_interval()
    }

    fn sample_mining_interval(&mut self) -> Suspension {
        Suspension::Timeout(max((self.dist.sample(&mut self.rng) * 1000.0) as u64, 1))
    }

    fn process_block(&mut self, block: Block, _env: &mut Environment<Block>) -> Suspension {
        // Track our own outputs as future spendable funds (coinbase + producer change).
        for tx in block.transactions.iter() {
            for (i, output) in tx.outputs.iter().enumerate() {
                if output.script_public_key.eq(&self.miner_data.script_public_key) {
                    if self.possible_unspent_outpoints.len() == self.max_cached_outpoints {
                        let evict =
                            (self.dist.sample(&mut self.rng) as usize) % self.max_cached_outpoints;
                        self.possible_unspent_outpoints.swap_remove_index(evict);
                    }
                    self.possible_unspent_outpoints
                        .insert(TransactionOutpoint::new(tx.id(), i as u32));
                }
            }
        }
        if self.report_progress() {
            Suspension::Halt
        } else {
            let session = self.consensus.acquire_session();
            let status = futures::executor::block_on(
                self.consensus.validate_and_insert_block(block).virtual_state_task,
            )
            .unwrap();
            assert!(status.is_utxo_valid_or_pending());
            drop(session);
            Suspension::Idle
        }
    }

    fn report_progress(&mut self) -> bool {
        self.num_blocks += 1;
        self.sim_time = self.sim_time.max(self.num_blocks);
        matches!(self.target_blocks, Some(t) if self.num_blocks > t)
    }
}

impl Process<Block> for L2Miner {
    fn resume(
        &mut self,
        resumption: Resumption<Block>,
        env: &mut Environment<Block>,
    ) -> Suspension {
        match resumption {
            Resumption::Initial => self.sample_mining_interval(),
            Resumption::Scheduled => {
                self.sim_time = env.now();
                self.mine(env)
            }
            Resumption::Message(block) => self.process_block(block, env),
        }
    }
}

/// A one-shot transaction selector that hands the consensus template builder a fixed tx list.
struct OnetimeTxSelector {
    txs: Option<Vec<Transaction>>,
}

impl OnetimeTxSelector {
    fn new(txs: Vec<Transaction>) -> Self {
        Self { txs: Some(txs) }
    }
}

impl TemplateTransactionSelector for OnetimeTxSelector {
    fn select_transactions(&mut self) -> Vec<Transaction> {
        self.txs.take().unwrap()
    }

    fn reject_selection(&mut self, tx_id: TransactionId) {
        // Standard template building rejects a selected tx only if it fails block-template
        // validation. The producer is expected to emit txs valid against the current virtual
        // state, so a rejection is a producer bug; surface which tx so the seeded run is debuggable
        // rather than panicking opaquely.
        panic!("producer emitted a tx the block template rejected: {tx_id}");
    }

    fn is_successful(&self) -> bool {
        true
    }
}
