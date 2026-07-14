//! Drives the real deposit/transfer/withdraw `runtime-processor` guest through the
//! `Scheduler` + `Vm` + `Backend`, scheduling carrier transactions directly (no mempool, no L1
//! node). This isolates the host->guest `Inputs::encode` seam: it proves the scheduler rebuilds the
//! exact blob the guest expects, that the genesis-gated config `Init` works, and that
//! deposit/transfer/withdraw mutate committed state as expected.
//!
//! Each `scheduler.schedule(..)` is one batch = one committed state version (1, 2, 3, ...). State
//! is read back from the store via `StateVersion::get(store, version, id)`.
//!
//! Runs under dev mode (`RISC0_DEV_MODE=1`), which still executes the guest (only the proof is
//! faked), so the asserted state transitions are real.

use kaspa_hashes::Hash;
use tempfile::TempDir;
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_scheduling_scheduler::{ExecutionConfig, Scheduler};
use vprogs_state_version::StateVersion;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_backend_risc0_api::{Backend, ProofType};
use vprogs_zk_backend_risc0_test_suite::{
    L1TransactionExt, batch_aggregator_elf, batch_processor_elf,
    runtime_flow::{
        EXAMPLE_DEPOSIT_COVENANT_ID, RuntimeSigner, config_id, config_min_withdrawal, deposit_tx,
        init_config_tx, rotate_user_lock_tx, transfer_create_tx, transfer_tx, update_config_tx,
        user_balance, withdraw_tx,
    },
    runtime_processor_elf,
};
use vprogs_zk_vm::{ProvingPipeline, Vm};

type FlowVm = Vm<Backend, RocksDbStore>;

/// A scheduler harness that commits one batch per scheduled transaction set and tracks the
/// resulting state version.
struct Harness {
    scheduler: Scheduler<RocksDbStore, FlowVm>,
    storage: RocksDbStore,
    version: u64,
    _temp: TempDir,
}

impl Harness {
    /// Builds a fresh harness over a temp RocksDB store, executing (not proving) the runtime ELF.
    fn new() -> Self {
        let temp = TempDir::new().expect("temp dir");
        let storage: RocksDbStore = RocksDbStore::open(temp.path());
        let backend = Backend::new(
            &runtime_processor_elf(),
            &batch_processor_elf(),
            &batch_aggregator_elf(),
            ProofType::Succinct,
        );
        // None: execute + commit only; no proving needed to verify state transitions.
        let vm = Vm::new(backend, ProvingPipeline::None);
        let scheduler = Scheduler::new(
            ExecutionConfig::default().with_processor(vm),
            StorageConfig::default().with_store(storage.clone()),
        );
        Self { scheduler, storage, version: 0, _temp: temp }
    }

    /// Schedules a single carrier tx as its own batch and blocks until committed. Returns the
    /// committed state version.
    fn commit(&mut self, tx: vprogs_l1_types::L1Transaction) -> u64 {
        self.version += 1;
        let meta = synthetic_metadata(self.version);
        let batch = self.scheduler.schedule(meta, vec![tx.into_scheduler_tx(0)]);
        batch.wait_committed_blocking();
        self.version
    }

    /// Schedules several carrier txs in one batch (one L1 block), in declared order, and blocks
    /// until committed. `merge_idx` is the tx's position. Returns the committed version.
    fn commit_block(&mut self, txs: Vec<vprogs_l1_types::L1Transaction>) -> u64 {
        self.version += 1;
        let meta = synthetic_metadata(self.version);
        let scheduled =
            txs.into_iter().enumerate().map(|(i, tx)| tx.into_scheduler_tx(i as u32)).collect();
        let batch = self.scheduler.schedule(meta, scheduled);
        batch.wait_committed_blocking();
        self.version
    }

    /// Reads a resource's latest committed bytes, or `None` if never written/empty.
    fn resource(&self, id: vprogs_core_types::ResourceId) -> Option<Vec<u8>> {
        let data = StateVersion::from_latest_data(&self.storage, id).data().clone();
        if data.is_empty() { None } else { Some(data) }
    }

    /// Decoded user balance for `signer`, or `None` if its resource is absent.
    fn balance(&self, signer: &RuntimeSigner) -> Option<u64> {
        self.resource(signer.user_id()).and_then(|b| user_balance(&b))
    }
}

/// A synthetic `ChainBlockMetadata` for batch `n`. The runtime ignores the mergeset context hash
/// derived from these fields; the scheduler only needs a distinct checkpoint per batch.
fn synthetic_metadata(n: u64) -> ChainBlockMetadata {
    ChainBlockMetadata {
        hash: Hash::from_bytes([n as u8; 32]),
        blue_score: n,
        daa_score: n,
        timestamp: n * 1000,
        prev_timestamp: n.saturating_sub(1) * 1000,
        ..Default::default()
    }
}

/// The happy path through every action: Init the config, deposit to Alice, transfer-create Bob,
/// plain transfer Alice->Bob, withdraw from Bob, and an Update of the config min.
#[test]
fn deposit_transfer_withdraw_happy_path() {
    let cov = EXAMPLE_DEPOSIT_COVENANT_ID;
    let min_withdrawal = 100;
    let owner = RuntimeSigner::genesis();
    let alice = RuntimeSigner::user(1);
    let bob = RuntimeSigner::user(2);

    let mut h = Harness::new();

    // 1. Init the singleton config (genesis-signed).
    let v = h.commit(init_config_tx(min_withdrawal, cov, &owner));
    let cfg = h.resource(config_id()).expect("config committed after Init");
    assert_eq!(config_min_withdrawal(&cfg), Some(min_withdrawal), "Init set min_withdrawal");
    eprintln!("[happy] config initialized at version {v}");

    // 2. Deposit 5_000 to Alice (creates her user resource).
    h.commit(deposit_tx(cov, &alice, 5_000));
    assert_eq!(h.balance(&alice), Some(5_000), "deposit credited Alice");

    // 3. Transfer-create Bob with 2_000 from Alice.
    h.commit(transfer_create_tx(&alice, &bob, 2_000));
    assert_eq!(h.balance(&alice), Some(3_000), "Alice debited by transfer-create");
    assert_eq!(h.balance(&bob), Some(2_000), "Bob created+credited by transfer");

    // 4. Plain transfer 1_000 Alice -> Bob (Bob already exists).
    h.commit(transfer_tx(&alice, &bob, 1_000));
    assert_eq!(h.balance(&alice), Some(2_000), "Alice debited by transfer");
    assert_eq!(h.balance(&bob), Some(3_000), "Bob credited by transfer");

    // 5. Withdraw 1_500 from Bob (emits an L2->L1 exit).
    h.commit(withdraw_tx(&bob, 1_500, &[0xAA; 32]));
    assert_eq!(h.balance(&bob), Some(1_500), "Bob debited by withdraw");

    // 6. Update the config min_withdrawal (owner-signed).
    let v = h.commit(update_config_tx(250, cov, &owner));
    let cfg = h.resource(config_id()).expect("config still present after Update");
    assert_eq!(config_min_withdrawal(&cfg), Some(250), "Update rotated min_withdrawal");
    eprintln!("[happy] all actions applied; final version {v}");
}

/// Rotating a user's lock preserves its balance and resource identity (the resource id derives from
/// the immutable `initial_lock_hash`, not the current lock).
#[test]
fn rotate_user_lock_preserves_balance() {
    let cov = EXAMPLE_DEPOSIT_COVENANT_ID;
    let owner = RuntimeSigner::genesis();
    let alice = RuntimeSigner::user(30);
    let alice_new = RuntimeSigner::user(31);

    let mut h = Harness::new();
    h.commit(init_config_tx(100, cov, &owner));
    h.commit(deposit_tx(cov, &alice, 5_000));
    assert_eq!(h.balance(&alice), Some(5_000));

    // Rotate alice's controlling key to alice_new. The resource stays at alice.user_id().
    h.commit(rotate_user_lock_tx(&alice, &alice_new));
    assert_eq!(h.balance(&alice), Some(5_000), "rotation must preserve balance + identity");
}

/// A transfer whose source has never been funded must be rejected by the runtime (the whole tx
/// reverts), leaving no state. This is the "transfer before deposit" out-of-order case: the
/// transfer settles first and is dropped; the later deposit still works.
#[test]
fn transfer_before_deposit_is_rejected() {
    let cov = EXAMPLE_DEPOSIT_COVENANT_ID;
    let owner = RuntimeSigner::genesis();
    let alice = RuntimeSigner::user(10);
    let bob = RuntimeSigner::user(11);

    let mut h = Harness::new();
    h.commit(init_config_tx(100, cov, &owner));

    // Alice transfers to Bob before being funded: source is an empty/new slot -> tx rejected.
    h.commit(transfer_create_tx(&alice, &bob, 1_000));
    assert_eq!(h.balance(&alice), None, "rejected transfer must not create Alice");
    assert_eq!(h.balance(&bob), None, "rejected transfer must not create Bob");

    // The later deposit still funds Alice normally.
    h.commit(deposit_tx(cov, &alice, 4_000));
    assert_eq!(h.balance(&alice), Some(4_000), "deposit after rejected transfer still works");
}

/// Deposit + transfer to the same new user within ONE block (the scheduler chains the shared
/// resource), proving cross-tx within-batch ordering works.
#[test]
fn deposit_then_transfer_in_one_block() {
    let cov = EXAMPLE_DEPOSIT_COVENANT_ID;
    let owner = RuntimeSigner::genesis();
    let alice = RuntimeSigner::user(20);
    let bob = RuntimeSigner::user(21);

    let mut h = Harness::new();
    h.commit(init_config_tx(100, cov, &owner));

    // One block: deposit to Alice, then Alice transfer-creates Bob. The transfer depends on the
    // deposit's write to Alice's resource, so the scheduler orders them.
    h.commit_block(vec![deposit_tx(cov, &alice, 6_000), transfer_create_tx(&alice, &bob, 2_500)]);
    assert_eq!(h.balance(&alice), Some(3_500), "Alice funded then debited within one block");
    assert_eq!(h.balance(&bob), Some(2_500), "Bob created within one block");
}
