//! Milestone 2: drive the deposit/transfer/withdraw runtime through the full L1 path on a real
//! kaspad simnet, with TWO framework nodes following the same chain via their bridges and reaching
//! identical committed state.
//!
//! Carriers ride the lane subnetwork ([`TEST_SUBNETWORK_ID`], `TX_VERSION_TOCCATA`) and are funded
//! from the node's coinbase wallet (the deposit funding output locks real KAS to the covenant
//! deposit address). Config `Init` is genesis-funded + witness-authorized (see flow-test-issues/01).
//!
//! Runs under dev mode (`RISC0_DEV_MODE=1`): the guest executes for real (only the proof is faked).

use std::time::{Duration, Instant};

use kaspa_consensus_core::{
    config::params::Params,
    constants::{SOMPI_PER_KASPA, TX_VERSION_TOCCATA},
    network::{NetworkId, NetworkType},
    tx::Transaction,
};
use tempfile::TempDir;
use vprogs_core_types::ResourceId;
use vprogs_l1_bridge::L1BridgeConfig;
use vprogs_l1_types::ChainBlockMetadata;
use vprogs_node_framework::{Node, NodeConfig};
use vprogs_node_test_utils::L1Node;
use vprogs_scheduling_scheduler::ExecutionConfig;
use vprogs_state_metadata::StateMetadata;
use vprogs_state_version::StateVersion;
use vprogs_storage_manager::StorageConfig;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_zk_backend_risc0_api::{Backend, ProofType};
use vprogs_zk_backend_risc0_test_suite::{
    batch_aggregator_elf, batch_processor_elf, force_covenant_forks, runtime_processor_elf,
    runtime_flow::{
        deposit_carrier, init_config_carrier, transfer_carrier, transfer_create_carrier,
        user_balance, withdraw_carrier, RuntimeSigner, EXAMPLE_DEPOSIT_COVENANT_ID,
    },
    TEST_SUBNETWORK_ID,
};
use vprogs_zk_vm::{ProvingPipeline, Vm};

type FlowVm = Vm<Backend, RocksDbStore>;
type FlowNode = Node<RocksDbStore, FlowVm>;

const TIMEOUT: Duration = Duration::from_secs(120);
const KAS: u64 = SOMPI_PER_KASPA;

/// Simnet params: fast coinbase maturity + the forks that activate Toccata / covenants so lane
/// (`TX_VERSION_TOCCATA`) carriers are accepted.
fn simnet_params(p: &mut Params) {
    p.blockrate.coinbase_maturity = 1;
    force_covenant_forks(p);
}

/// Builds a framework node following `l1`, executing the deposit/transfer/withdraw runtime ELF and
/// committing to its own RocksDB store (returned so the test can read state). Execution-only
/// (`ProvingPipeline::None`).
fn create_flow_node(l1: &L1Node, temp: &TempDir) -> (FlowNode, RocksDbStore) {
    let store: RocksDbStore = RocksDbStore::open(temp.path());
    let backend = Backend::new(
        &runtime_processor_elf(),
        &batch_processor_elf(),
        &batch_aggregator_elf(),
        ProofType::Succinct,
    );
    let vm = Vm::new(backend, ProvingPipeline::None);
    let node = Node::new(
        NodeConfig::default()
            .with_execution_config(ExecutionConfig::default().with_processor(vm))
            .with_storage_config(StorageConfig::default().with_store(store.clone()))
            .with_l1_bridge_config(
                L1BridgeConfig::default()
                    .with_url(Some(l1.wrpc_borsh_url()))
                    .with_network_type(NetworkType::Simnet)
                    .with_subnetwork_id(Some(TEST_SUBNETWORK_ID)),
            ),
    );
    (node, store)
}

/// Polls `store` until `id`'s decoded user balance equals `expected`, or panics on timeout.
fn wait_balance(store: &RocksDbStore, id: ResourceId, expected: u64) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let got = read_balance(store, id);
        if got == Some(expected) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timeout waiting for balance {expected} on {id:?}, got {got:?}",
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn read_balance(store: &RocksDbStore, id: ResourceId) -> Option<u64> {
    let data = StateVersion::from_latest_data(store, id).data().clone();
    if data.is_empty() { None } else { user_balance(&data) }
}

/// Committed batch index persisted to `store` (number of chain blocks processed).
fn committed_index(store: &RocksDbStore) -> u64 {
    StateMetadata::last_committed::<ChainBlockMetadata, _>(store).index()
}

#[tokio::test(flavor = "multi_thread")]
async fn two_nodes_agree_on_deposit_transfer_withdraw_flow() {
    let cov = EXAMPLE_DEPOSIT_COVENANT_ID;
    let min_withdrawal = 100;
    let owner = RuntimeSigner::genesis(); // config owner = genesis (simple); any key works
    let alice = RuntimeSigner::user(1);
    let bob = RuntimeSigner::user(2);
    let carol = RuntimeSigner::user(3);
    let dave = RuntimeSigner::user(4);

    let l1 = L1Node::new(NetworkId::new(NetworkType::Simnet), Some(simnet_params)).await;
    // Plenty of mature coinbase UTXOs to fund the carriers (one per carrier).
    l1.mine_utxos(16).await;

    // 1. Bootstrap config: a normal genesis-signed Init carrier, funded from the coinbase wallet.
    mine_carrier(&l1, init_config_carrier(min_withdrawal, cov, &owner)).await;

    // 2. Deposit Alice (5 KAS) and Bob (3 KAS).
    mine_carrier(&l1, deposit_carrier(cov, &alice, 5 * KAS)).await;
    mine_carrier(&l1, deposit_carrier(cov, &bob, 3 * KAS)).await;

    // 3. Transfer 1 KAS Alice -> Bob.  (Alice 4, Bob 4)
    mine_carrier(&l1, transfer_carrier(&alice, &bob, 1 * KAS)).await;

    // 4. Withdraw 2 KAS from Bob (emits an exit).  (Bob 2)
    mine_carrier(&l1, withdraw_carrier(&bob, 2 * KAS, &[0xAA; 32])).await;

    // 5. Out-of-order: Carol (unfunded) transfer-creates Dave -> runtime REJECTS the whole tx.
    mine_carrier(&l1, transfer_create_carrier(&carol, &dave, 1 * KAS)).await;

    // 6. Deposit Carol (2 KAS) -> the observable success after the rejection.
    mine_carrier(&l1, deposit_carrier(cov, &carol, 2 * KAS)).await;

    // Start both nodes; each replays the whole chain from genesis through its own bridge.
    let temp_a = TempDir::new().unwrap();
    let temp_b = TempDir::new().unwrap();
    let (node_a, store_a) = create_flow_node(&l1, &temp_a);
    let (node_b, store_b) = create_flow_node(&l1, &temp_b);

    // Wait until both nodes have processed through Carol's deposit (the last action).
    for store in [&store_a, &store_b] {
        wait_balance(store, carol.user_id(), 2 * KAS);
    }

    // Both nodes must agree on the full final state.
    for (label, store) in [("A", &store_a), ("B", &store_b)] {
        assert_eq!(read_balance(store, alice.user_id()), Some(4 * KAS), "node {label}: Alice");
        assert_eq!(read_balance(store, bob.user_id()), Some(2 * KAS), "node {label}: Bob");
        assert_eq!(read_balance(store, carol.user_id()), Some(2 * KAS), "node {label}: Carol");
        assert_eq!(read_balance(store, dave.user_id()), None, "node {label}: Dave (rejected)");
        eprintln!("[two-node] node {label} committed index = {}", committed_index(store));
    }

    node_a.shutdown();
    node_b.shutdown();
    l1.shutdown().await;
}

/// Builds a funded lane carrier for `carrier` from the node's coinbase wallet and mines it (+1
/// acceptance block).
async fn mine_carrier(
    l1: &L1Node,
    carrier: vprogs_zk_backend_risc0_test_suite::runtime_flow::RuntimeCarrier,
) {
    let tx = l1
        .wallet()
        .build_signed_carrier(
            carrier.extra_outputs().to_vec(),
            TEST_SUBNETWORK_ID,
            TX_VERSION_TOCCATA,
            |rest: &[u8]| carrier.payload(rest),
        )
        .await;
    mine_and_accept(l1, &[tx]).await;
}

/// Mines `txs` in one block, then mines an acceptance block so they enter the selected chain.
async fn mine_and_accept(l1: &L1Node, txs: &[Transaction]) {
    l1.mine_block(txs).await;
    l1.mine_blocks(1).await;
}
