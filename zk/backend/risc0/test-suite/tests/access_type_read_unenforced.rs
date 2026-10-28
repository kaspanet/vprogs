//! Reproduces #107 end to end: a transaction whose sender-supplied declaration marks a resource
//! `AccessType::Read` while the guest program writes it.
//!
//! The declaration is decoded verbatim from the L1 payload, so any sender can misdeclare a
//! correct program's write set. The guest framework therefore rejects such a transaction
//! whole (`ErrorCode::ReadDeclaredWrite`) instead of journaling a write the host store (which
//! honors the declaration) would silently drop: that divergence permanently wedged every later
//! bundle on the aggregator's `prev_state` assert, with no in-protocol recovery.
//!
//! Both tests run the real `Vm`, the real risc0 transaction-processor guest, the real
//! batch-processor guest, the real batch prover and the real scheduler, against a real simnet L1
//! node. No mock processor is involved: the two shared mocks gate their writes on
//! `access_type == Write`, which is exactly what the real guest does not do, so a mock-based
//! test of this contract passes vacuously.

use kaspa_consensus_core::network::{NetworkId, NetworkType};
use kaspa_hashes::Hash;
use kaspa_rpc_core::api::rpc::RpcApi;
use tempfile::TempDir;
use vprogs_core_smt::{EMPTY_HASH, Tree as _};
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId};
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};
use vprogs_node_test_utils::L1Node;
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_state_version::StateVersion;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_abi::{
    Error, ErrorCode,
    batch_processor::BatchTransition,
    transaction_processor::{JournalEntries, OutputCommitment},
};
use vprogs_zk_backend_risc0_api::{Backend, ProofType, Receipt};
use vprogs_zk_backend_risc0_test_suite::{
    L1TransactionExt, aggregate_batches, batch_aggregator_elf, batch_processor_elf,
    compute_section_lane_tip, test_lane_key, transaction_processor_elf,
};
use vprogs_zk_batch_prover::{Backend as _, BatchProverConfig};
use vprogs_zk_vm::{ProvingPipeline, Vm};
use zerocopy::FromBytes;

/// Resource the sender declares `Read` and the dummy guest writes anyway (it increments every
/// resource it is handed).
const TARGET: usize = 1;

/// The real `Vm` over the real risc0 guests, plus the simnet node its prover reads lane proofs
/// from.
struct Fixture {
    /// Backend wrapping the three real guest ELFs.
    backend: Backend,
    /// Simnet node the batch prover fetches lane proofs from.
    l1: L1Node,
    /// The host store whose root the settled state is compared against.
    storage: RocksDbStore,
    /// Scheduler driving the real `Vm`.
    scheduler: Scheduler<RocksDbStore, Vm<Backend, RocksDbStore>>,
    /// Mined simnet blocks batch metadata is anchored to.
    block_hashes: Vec<Hash>,
    /// Backing directory for `storage`, dropped at end of test.
    _temp_dir: TempDir,
}

impl Fixture {
    /// Builds the fixture and mines `blocks` simnet blocks to anchor batch metadata against.
    async fn new(blocks: usize) -> Self {
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let storage: RocksDbStore = RocksDbStore::open(temp_dir.path());

        let backend = Backend::new(
            &transaction_processor_elf(),
            &batch_processor_elf(),
            &batch_aggregator_elf(),
            ProofType::Succinct,
        );

        let l1 = L1Node::new(NetworkId::new(NetworkType::Simnet), None).await;
        let block_hashes = l1.mine_blocks(blocks).await;

        let config = BatchProverConfig {
            lane_key: test_lane_key(),
            covenant_id: Hash::default(),
            deposit_spk_hash: [0u8; 32],
        };

        let vm = Vm::new(
            backend.clone(),
            ProvingPipeline::batch(backend.clone(), storage.clone(), config),
        );
        let scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(vm),
            StorageConfig::default().with_store(storage.clone()),
        );

        Self { backend, l1, storage, scheduler, block_hashes, _temp_dir: temp_dir }
    }

    /// Builds a `ChainBlockMetadata` from the mined simnet block at `idx`.
    async fn metadata(&self, idx: usize) -> ChainBlockMetadata {
        let block = self
            .l1
            .grpc_client()
            .get_block(self.block_hashes[idx], false)
            .await
            .expect("get_block");
        let h = block.header;

        ChainBlockMetadata {
            hash: h.hash,
            blue_score: h.blue_score,
            daa_score: h.daa_score,
            timestamp: h.timestamp,
            seq_commit: h.accepted_id_merkle_root,
            ..Default::default()
        }
    }

    /// Schedules one L1 carrier transaction anchored to `metadata`, and returns the proven
    /// per-batch receipt plus the per-tx journals.
    async fn settle_one(
        &mut self,
        metadata: ChainBlockMetadata,
        tx: L1Transaction,
    ) -> (Receipt, Vec<Vec<u8>>) {
        let batch = self.scheduler.schedule(metadata, vec![tx.into_scheduler_tx(0)]);
        batch.wait_committed_blocking();
        batch.wait_artifact_published_blocking();

        let receipt = (*batch.artifact()).clone();
        let journals = batch.tx_artifacts().map(|a| Backend::journal_bytes(&a)).collect();
        (receipt, journals)
    }
}

/// Decodes a per-batch receipt's journal into its `(prev_state, new_state)`.
fn state_transition(receipt: &Receipt) -> ([u8; 32], [u8; 32]) {
    let journal = Backend::journal_bytes(receipt);
    let t = BatchTransition::ref_from_bytes(&journal).expect("decode BatchTransition");

    (t.prev_state, t.new_state)
}

/// A misdeclared transaction is rejected whole, and the settled state stays consistent with the
/// host store (#107).
///
/// Pre-fix, the guest journaled `Changed(hash(new_data))` for the dirty `Read`-declared resource,
/// the batch processor folded it into `new_state`, and the host silently dropped the write: the
/// batch's own proof succeeded while the two state views permanently disagreed. The fix rejects
/// the transaction inside the guest framework, so the journal carries the rejection, no state
/// changes, and both views agree on the empty root.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_declared_write_rejects_and_keeps_state_consistent() {
    let mut fx = Fixture::new(1).await;

    let metadata = fx.metadata(0).await;
    let tx = L1Transaction::for_l2_test(
        &[AccessMetadata::read(ResourceId::for_test(TARGET))],
        &[1, 2, 3],
    );
    let (receipt, journals) = fx.settle_one(metadata, tx).await;

    // The offending tx rejected through the journal with the dedicated error code.
    let entries = JournalEntries::decode(&journals[0]).expect("tx journal decodes");
    assert!(matches!(
        entries.output_commitment,
        OutputCommitment::Error(Error::Guest(code)) if code == ErrorCode::ReadDeclaredWrite as u32
    ));

    // No state transition: prev and new are both the empty root, host and settled view agree.
    let (prev_state, new_state) = state_transition(&receipt);
    assert_eq!(prev_state, EMPTY_HASH, "prev_state should be empty (no prior state)");
    assert_eq!(new_state, EMPTY_HASH, "a rejected tx must not move the settled state");
    assert_eq!(
        StateVersion::get(&fx.storage, 1, &ResourceId::for_test(TARGET)),
        None,
        "host store holds no version 1 data: the rejected write never landed"
    );
    assert_eq!(
        new_state,
        fx.storage.root(1),
        "settled new_state must equal the host store's root for the same batch"
    );

    fx.scheduler.shutdown();
}

/// A bundle must aggregate cleanly after any batch the pipeline produced.
///
/// Pre-fix, the divergent batch's settled `new_state` recorded the guest's write while the next
/// batch's `prev_state` was proved from the host SMT (which never recorded it), so the
/// aggregator's `assert_eq!(this.prev_state, prev.new_state)` fired and the lane was permanently
/// wedged: the offending L1 transaction stays in the chain, so replay re-derived the divergence.
/// With the rejection, the state chain is intact and the bundle aggregates.
#[tokio::test(flavor = "multi_thread")]
async fn bundle_must_aggregate_after_a_read_declared_write() {
    let mut fx = Fixture::new(2).await;
    let lane_key = test_lane_key();

    // Batch 1: the misdeclared transaction. It rejects; the batch settles the empty root. The
    // carrier is bound (and cloned into the scheduler) so the lane tip can be derived below.
    let metadata_1 = fx.metadata(0).await;
    let carrier = L1Transaction::for_l2_test(
        &[AccessMetadata::read(ResourceId::for_test(TARGET))],
        &[1, 2, 3],
    );
    let (offending, _) = fx.settle_one(metadata_1, carrier.clone()).await;

    // Batch 2: any subsequent transaction touching the same resource under a matching
    // declaration. Its prev_state is proved from the host SMT. Production chains the lane
    // state block to block via the bridge; tests bypass the bridge, so chain those fields
    // manually with the same `lane_tip_next` derivation the guest uses. A rejected tx still
    // contributes its activity leaf, so the rejected batch advances the lane tip normally.
    let mut metadata_2 = fx.metadata(1).await;
    metadata_2.prev_lane_tip = compute_section_lane_tip(&metadata_1, &[(0, &carrier)], &lane_key);
    metadata_2.prev_lane_blue_score = metadata_1.blue_score;
    let next_tx = L1Transaction::for_l2_test(
        &[AccessMetadata::write(ResourceId::for_test(TARGET))],
        &[4, 5, 6],
    );
    let (next, _) = fx.settle_one(metadata_2, next_tx).await;

    // The state chain is intact: batch 2 starts exactly where batch 1 settled.
    let (_, offending_new_state) = state_transition(&offending);
    let (next_prev_state, _) = state_transition(&next);
    assert_eq!(
        next_prev_state, offending_new_state,
        "the rejected tx must not break the state chain"
    );

    // The invariant: the bundle aggregates.
    let bundle = aggregate_batches(
        &fx.backend,
        fx.l1.grpc_client(),
        &test_lane_key(),
        fx.block_hashes[1],
        vec![offending, next],
    )
    .await;
    assert!(!Backend::journal_bytes(&bundle).is_empty(), "bundle receipt should carry a journal");

    fx.scheduler.shutdown();
}
