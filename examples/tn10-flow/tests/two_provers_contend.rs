//! End-to-end two-prover contention test (dev mode, CPU).
//!
//! Two independent prover nodes settle the SAME dev covenant against ONE simnet L1, racing from
//! different fee addresses with different bundle-size ranges, advancing each other. This proves the
//! settler's competitor-reconciliation resilience: under contention neither worker panics, both
//! provers land settlements, and the covenant continuation chain stays a single contiguous
//! spend-chain (the two provers cooperatively advance ONE covenant rather than forking it).
//!
//! Runs only under `RISC0_DEV_MODE=1` (dev stub proofs + dev redeem; no GPU). The production /
//! CUDA path is covered by `zk/backend/risc0/test-suite/tests/settlement_l1_e2e.rs`.

use std::{collections::HashMap, ops::RangeInclusive, sync::Arc, time::Duration};

use kaspa_addresses::{Address, Prefix, Version};
use kaspa_consensus_core::{
    config::params::{ForkActivation, Params},
    constants::{SOMPI_PER_KASPA, TX_VERSION_TOCCATA},
    mass::BlockMassLimits,
    network::{NetworkId, NetworkType},
    subnets::SubnetworkId,
    tx::{Transaction, TransactionOutpoint},
};
use kaspa_hashes::Hash;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_txscript::pay_to_address_script;
use kaspa_wrpc_client::prelude::*;
use secp256k1::Keypair;
use tempfile::TempDir;
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_core_smt::EMPTY_HASH;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId};
use vprogs_example_tn10_flow::daemon::{self, BridgeParams, Elfs, FlowNode, ProvingParams};
use vprogs_l1_types::L1TransactionCovenantExt;
use vprogs_l1_wallet::encode_activity_payload;
use vprogs_node_test_utils::L1Node;
use vprogs_zk_backend_risc0_api::{Backend, ProofType};
use vprogs_zk_backend_risc0_settler::{
    AlternationPacer, CovenantState, SettlementMode, SettlementWorkerConfig, dev_bootstrap_redeem,
    run as run_settlement_worker,
};
use vprogs_zk_backend_risc0_test_suite::{
    TEST_SUBNETWORK_ID, batch_aggregator_elf, batch_processor_elf, dev_mode_enabled, test_lane_key,
    transaction_processor_elf,
};

/// Value locked in the covenant UTXO at bootstrap (1 KAS), matching the e2e settlement tests.
const COVENANT_VALUE: u64 = SOMPI_PER_KASPA;

/// Lane subnetwork both provers route their L2 carrier txs onto: the shared [`TEST_SUBNETWORK_ID`]
/// the guest derives its lane_key from, so the chain's lane-key bucket and the guest's committed
/// lane_key match (see the e2e test's `L2_LANE_SUBNET` doc for the divergence this avoids).
const LANE_SUBNET: SubnetworkId = TEST_SUBNETWORK_ID;

/// Bridge lane-finality window. Consensus only falls back to the parent block's `seq_commit`
/// (rather than the lane tip) as the lane parent-ref when the lane has no live SMT entry: on its
/// FIRST activation, or after it has gone silent past the real finality window (~432k blocks, never
/// reached in a test). The bridge models "no live entry" as `blue_score - parent.lane_blue_score >
/// finality_depth`. With the real simnet finality_depth the first activation never trips that
/// bound, so the bridge would pick the zero parent lane tip while consensus picks the parent
/// seq_commit, and the guest's committed `new_seq_commit` diverges from the chain's
/// `accepted_id_merkle_root`. A small window fixes this: the first carrier's blue score is well
/// above it (parent `lane_blue_score` is 0, so first activation expires → seq_commit, matching
/// consensus), while consecutive carriers stay within it (gap ~2 blue score → lane alive → lane
/// tip, matching consensus, which never expires a re-touched lane in-test).
const LANE_FINALITY_DEPTH: u64 = 10;

/// Sompi each funding output carries, and how many each prover address is seeded with. A prover's
/// settler funds every settlement fee from its own address, so it needs several spendable UTXOs to
/// fund a chain of settlements under contention.
const FUND_VALUE: u64 = 100_000_000;
const FUND_COUNT: usize = 6;

/// Per-prover wiring kept alive for the duration of the run. Dropping the [`FlowNode`] tears the
/// prover's bridge and pipeline down, so the test holds both for the whole driver loop.
struct Prover {
    /// The proving node (bridge + pipeline). Explicitly shut down at teardown so its worker, and
    /// the RocksDB store it holds, is released before `_db_dir` is reclaimed.
    node: FlowNode,
    /// The settler task. Awaited at the end; `Ok(())` proves it did not panic on a competitor.
    settler: tokio::task::JoinHandle<()>,
    /// Latch the test opens to tear the settler down gracefully.
    shutdown: AtomicAsyncLatch,
    /// Scratch dir backing the prover's RocksDB store. Held for the run, then reclaimed on drop
    /// after the node is shut down so the store has already closed its files.
    _db_dir: TempDir,
}

#[tokio::test(flavor = "multi_thread")]
async fn two_provers_contend() {
    // Dev-only: real proofs need a GPU. Under non-dev builds this test has nothing to assert.
    if !dev_mode_enabled() {
        eprintln!(
            "skipping two_provers_contend: RISC0_DEV_MODE!=1 - the contention scenario runs dev \
             stub proofs + the dev redeem on CPU; the production path is covered by \
             settlement_l1_e2e under CUDA",
        );
        return;
    }

    // === Step 0: simnet L1 ===
    // Mirror the e2e settlement config: instant coinbase maturity, covenants always active, a
    // raised block mass cap (the dev settlements are small but the cap keeps us clear of the
    // default 500k).
    let l1 = L1Node::new(
        NetworkId::new(NetworkType::Simnet),
        Some(|p| {
            p.blockrate.coinbase_maturity = 1;
            p.toccata_activation = ForkActivation::always();
            p.prior_block_mass_limits = BlockMassLimits::with_shared_limit(2_000_000);
        }),
    )
    .await;
    // Coinbase feeds the bootstrap, both funding txs, and ongoing carrier fees; mine generously.
    l1.mine_utxos(30).await;

    let network_id = NetworkId::new(NetworkType::Simnet);
    let lane_key = test_lane_key();

    // === Step 1: bootstrap the shared dev covenant ===
    let (bootstrap_redeem, bootstrap_spk) = dev_bootstrap_redeem(&lane_key);
    let (boot_tx, covenant_id) =
        l1.build_covenant_bootstrap_transaction(&bootstrap_redeem, COVENANT_VALUE).await;
    let boot_txid = boot_tx.id();
    let block_deploy = l1.mine_block(&[boot_tx]).await;
    l1.mine_blocks(1).await;
    eprintln!("dev covenant bootstrapped: covenant_id={covenant_id} block_deploy={block_deploy}");
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The settler's `run` confirms this outpoint and fills daa_score itself; we hand each prover
    // its own clone of the initial state.
    let bootstrap_outpoint = TransactionOutpoint::new(boot_txid, 0);
    let initial_covenant = CovenantState {
        covenant_id,
        state: EMPTY_HASH,
        lane_tip: Hash::default(),
        outpoint: bootstrap_outpoint,
        spk: bootstrap_spk,
        value: COVENANT_VALUE,
        daa_score: 0,
    };

    // === Step 2: two distinct prover keypairs, each funded at its own address ===
    let kp_a = Keypair::new(secp256k1::SECP256K1, &mut secp256k1::rand::thread_rng());
    let kp_b = Keypair::new(secp256k1::SECP256K1, &mut secp256k1::rand::thread_rng());
    let addr_a = prover_address(&kp_a, network_id);
    let addr_b = prover_address(&kp_b, network_id);
    assert_ne!(addr_a, addr_b, "the two provers must settle from distinct addresses");
    l1.fund_address(&addr_a, FUND_VALUE, FUND_COUNT).await;
    l1.fund_address(&addr_b, FUND_VALUE, FUND_COUNT).await;
    eprintln!("funded prover A address {addr_a}");
    eprintln!("funded prover B address {addr_b}");

    // === Step 3: spin up both provers ===
    // Both bundle over the same size range, so neither structurally forms a bundle first. The
    // settlers share an `AlternationPacer`: whoever lands a settlement waits for the other to land
    // the next, so they strictly alternate. The deferring prover finds its covenant outpoint
    // already spent (its bundle superseded), skips it, and reconciles to the other's advance before
    // settling the following range. This keeps the spend-chain a single contiguous chain that both
    // provers advance, and makes the both-provers-settled outcome deterministic instead of a
    // per-range coin flip that can starve one prover.
    let tx_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();
    let aggregator_elf = batch_aggregator_elf();
    let elfs = Elfs { transaction: &tx_elf, batch: &batch_elf, aggregator: &aggregator_elf };
    let params = Params::from(network_id);
    let pacer = Arc::new(AlternationPacer::new());

    let prover_a = spawn_prover(
        &l1,
        "A",
        kp_a,
        addr_a.clone(),
        2..=4,
        network_id,
        &params,
        lane_key,
        covenant_id,
        initial_covenant.clone(),
        elfs,
        (0, pacer.clone()),
    )
    .await;
    let prover_b = spawn_prover(
        &l1,
        "B",
        kp_b,
        addr_b.clone(),
        2..=4,
        network_id,
        &params,
        lane_key,
        covenant_id,
        initial_covenant.clone(),
        elfs,
        (1, pacer.clone()),
    )
    .await;

    // === Step 4: drive lane activity ===
    // Each iteration opens ONE settlement range: it mines a full bundle's worth of lane carriers
    // (`CARRIERS_PER_RANGE`, the bundle minimum), then several acceptance blocks, then sleeps long
    // enough for BOTH provers to prove, race, settle, and confirm that range before the next opens.
    // Pacing one range at a time (rather than streaming carriers) keeps neither prover perpetually
    // a confirmation-latency behind the other: each range is a fresh, jittered spend race, so
    // over many ranges both provers win some. Settlements the settlers submit to the mempool
    // are pulled into the next mined block.
    const DRIVER_ITERS: usize = 16;
    const CARRIERS_PER_RANGE: usize = 2;
    for i in 0..DRIVER_ITERS {
        eprintln!("driver: top of iteration {i}");
        for _ in 0..CARRIERS_PER_RANGE {
            let payload = encode_activity_payload(
                &[AccessMetadata::write(ResourceId::for_test(1))],
                &[1, 2, 3],
            );
            let carrier = match tokio::time::timeout(
                Duration::from_secs(10),
                l1.build_subnet_payload_transactions(
                    vec![payload],
                    LANE_SUBNET,
                    TX_VERSION_TOCCATA,
                ),
            )
            .await
            {
                Ok(txs) => txs.into_iter().next().expect("carrier tx"),
                Err(_) => {
                    panic!("driver: build_subnet_payload_transactions timed out at iteration {i}")
                }
            };
            l1.mine_block(std::slice::from_ref(&carrier)).await;
        }
        // Mine acceptance blocks. Each acceptance block pulls pending settlements out of the
        // mempool and onto the chain, where both bridges observe them as the covenant's
        // `last_settlement`. The trailing prover then ADOPTS the advanced covenant instead
        // of orphaning a stale settlement against it.
        eprintln!("driver: iteration {i} mined carriers, mining acceptance");
        for _ in 0..5 {
            l1.mine_blocks(1).await;
            tokio::time::sleep(Duration::from_millis(800)).await;
        }
        eprintln!("driver: iteration {i} mined acceptance");
        if i % 5 == 0 {
            eprintln!("driver: iteration {i}/{DRIVER_ITERS}");
        }
    }

    // === Step 5: drain in-flight settlements, then tear the settlers down ===
    // Keep mining acceptance blocks so settlements still in the mempool land and confirm, using the
    // SAME one-block-at-a-time pacing as the driver loop's acceptance phase: mine one block, then
    // pause long enough for the trailing prover to adopt the advance before the next block lands.
    // Mining several blocks back-to-back here would let whichever prover is ahead settle the whole
    // backlog before the other adopts, starving it and making the both-provers-settled assertion
    // flaky. Poll the covenant chain and stop once it stops growing for a few rounds (or a bounded
    // ceiling), so the tail settlements are captured with the race still alternating.
    let mut prev_len = 0usize;
    let mut stable_rounds = 0;
    for round in 0..40 {
        l1.mine_blocks(1).await;
        tokio::time::sleep(Duration::from_millis(800)).await;
        let len = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        if len == prev_len {
            stable_rounds += 1;
        } else {
            stable_rounds = 0;
        }
        prev_len = len;
        if stable_rounds >= 3 && len >= 3 {
            break;
        }
        if round % 5 == 0 {
            eprintln!("drain: round {round}, covenant chain length {len}");
        }
    }

    prover_a.shutdown.open();
    prover_b.shutdown.open();
    let join_a = prover_a.settler.await;
    let join_b = prover_b.settler.await;

    // Tear the proving nodes down before their scratch dirs are reclaimed: `shutdown` joins each
    // node's worker, releasing the RocksDB store, so the held `TempDir` is removed only after the
    // store has closed its files. Done before the assertions below so a failing assertion still
    // unwinds cleanly rather than racing the directory removal at drop.
    prover_a.node.shutdown();
    prover_b.node.shutdown();

    // === Assertion 1: no panic (the resilience claim) ===
    // A settler that hit the old panic-on-competitor-advance returns Err (the task panicked).
    // Surface the join result before any other assertion so a panic shows up here, not as a
    // timeout.
    assert!(join_a.is_ok(), "prover A settler panicked: {join_a:?}");
    assert!(join_b.is_ok(), "prover B settler panicked: {join_b:?}");

    // === Assertion 2 + 3: contiguous chain + both provers settled ===
    let chain = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await;
    let change_spk_a = pay_to_address_script(&addr_a);
    let change_spk_b = pay_to_address_script(&addr_b);

    let mut per_prover: HashMap<&'static str, usize> = HashMap::new();
    let mut expected_input = bootstrap_outpoint;
    for (pos, link) in chain.iter().enumerate() {
        // Attribute by the settlement's change output: each prover funds its fee from its own key,
        // so its change output pays back to its own address.
        let attributed = if link.change_spks.contains(&change_spk_a) {
            "A"
        } else if link.change_spks.contains(&change_spk_b) {
            "B"
        } else {
            "?"
        };
        *per_prover.entry(attributed).or_default() += 1;
        eprintln!(
            "settlement #{pos}: tx={} prover={attributed} input={} output=({}:0)",
            link.tx_id, link.covenant_input, link.tx_id,
        );

        // === Assertion 3: contiguity ===
        // Each settlement's covenant input must be the previous settlement's covenant output
        // (output 0), starting from the bootstrap outpoint. A single linear spend-chain proves the
        // two provers advanced ONE covenant cooperatively with no divergent fork / double-settle.
        assert_eq!(
            link.covenant_input, expected_input,
            "settlement #{pos} ({}) must spend the previous covenant output {expected_input}; \
             the continuation chain forked",
            link.tx_id,
        );
        expected_input = TransactionOutpoint::new(link.tx_id, 0);
    }

    let count_a = *per_prover.get("A").unwrap_or(&0);
    let count_b = *per_prover.get("B").unwrap_or(&0);
    eprintln!(
        "settlements: total={} A={count_a} (addr {addr_a}) B={count_b} (addr {addr_b})",
        chain.len(),
    );

    // The chain must have actually advanced under contention.
    assert!(
        chain.len() >= 3,
        "expected a covenant chain of at least 3 settlements under contention, got {}",
        chain.len(),
    );
    // Both provers must have landed at least one settlement (proves the race, not a single winner).
    assert!(count_a >= 1, "prover A (addr {addr_a}) produced no settlements");
    assert!(count_b >= 1, "prover B (addr {addr_b}) produced no settlements");

    l1.shutdown().await;
}

/// One link in the covenant continuation chain: a settlement tx, the covenant outpoint its input 0
/// spends, and the SPKs of all its outputs (so attribution can match a prover's change SPK).
struct CovenantLink {
    /// The settlement transaction's id.
    tx_id: Hash,
    /// Covenant outpoint this settlement's input 0 spends.
    covenant_input: TransactionOutpoint,
    /// SPKs of all outputs, used to attribute the settlement to a prover by its change SPK.
    change_spks: Vec<kaspa_consensus_core::tx::ScriptPublicKey>,
}

/// Derives a prover's P2PK funding address from its keypair under the network's prefix, matching
/// how the wallet / settler derive the address they fund fees from.
fn prover_address(keypair: &Keypair, network_id: NetworkId) -> Address {
    let (xonly, _parity) = keypair.x_only_public_key();
    Address::new(Prefix::from(network_id.network_type()), Version::PubKey, &xonly.serialize())
}

/// Builds, wires, and starts one prover: a proving [`FlowNode`] over a fresh store + wRPC client,
/// and a spawned settler draining the node's settlement queue against the shared covenant. Returns
/// the assembled [`Prover`] (node kept alive, settler handle + shutdown latch for teardown).
#[allow(clippy::too_many_arguments)]
async fn spawn_prover(
    l1: &L1Node,
    label: &'static str,
    keypair: Keypair,
    address: Address,
    bundle_size: RangeInclusive<usize>,
    network_id: NetworkId,
    params: &Params,
    lane_key: Hash,
    covenant_id: Hash,
    covenant: CovenantState,
    elfs: Elfs<'_>,
    turn: (u8, Arc<AlternationPacer>),
) -> Prover {
    let wrpc_url = l1.wrpc_borsh_url();
    // Separate wRPC clients for the lane source and the settler so they own independent handles.
    let client_for_lane = connect_wrpc(&wrpc_url, network_id).await;
    let client_for_settler = connect_wrpc(&wrpc_url, network_id).await;

    let db_dir = TempDir::new().expect("temp dir");
    let store = daemon::Store::open(db_dir.path());

    let queue = daemon::FlowSettlementQueue::new();
    let node = daemon::build_proving_node(
        elfs,
        store,
        BridgeParams {
            url: wrpc_url,
            network_id,
            lane_subnet: LANE_SUBNET,
            covenant_id,
            finality_depth: LANE_FINALITY_DEPTH,
            seed_depth: 0,
            tip_daa: None,
        },
        ProvingParams {
            covenant_id,
            lane_key,
            client: client_for_lane,
            sink: queue.clone(),
            bundle_size,
        },
    );

    // Backend is required by the settler config; in Dev mode it pins no image ids but the type is
    // still threaded through.
    let backend = Backend::new(elfs.transaction, elfs.batch, elfs.aggregator, ProofType::Succinct);
    let shutdown = AtomicAsyncLatch::new();
    let settler = tokio::spawn(run_settlement_worker(
        queue,
        SettlementWorkerConfig {
            client: client_for_settler,
            params: params.clone(),
            keypair,
            lane_key,
            backend,
            mode: SettlementMode::Dev,
            // Jitter each submission so neither prover deterministically wins the spend race for
            // every range; without it the first-spawned prover lands every settlement. The window
            // is wide relative to the dev proving time so the per-range winner is a
            // genuine coin flip.
            submit_jitter: Some(0..40),
            // Strictly alternate with the competing prover: after one lands a settlement it waits
            // for the other to land the next, so neither sweeps every range (and each settles at
            // half rate, letting its recycled fee-change UTXO confirm before reuse). This makes the
            // both-provers-settled assertion deterministic rather than a coin flip per range.
            alternation: Some(turn),
        },
        covenant,
        shutdown.clone(),
    ));

    eprintln!("prover {label} started (addr {address})");
    Prover { node, settler, shutdown, _db_dir: db_dir }
}

/// Walks the selected-parent chain from the virtual tip down to `block_deploy`, collecting the
/// covenant continuation chain: the linear sequence of settlement txs that spend the bootstrap
/// outpoint and each other's output 0. Returns the links in chain order (bootstrap-spend first).
///
/// The driver is the only miner, so every block builds on the chain tip and the selected-parent
/// walk covers every settlement. We index settlements by the covenant outpoint they spend, then
/// thread from `bootstrap_outpoint` forward, which yields a single linear chain when (and only
/// when) the settlements never diverge.
async fn covenant_chain(
    l1: &L1Node,
    block_deploy: Hash,
    bootstrap_outpoint: TransactionOutpoint,
    covenant_id: Hash,
) -> Vec<CovenantLink> {
    // Map: covenant outpoint spent -> the settlement link that spends it.
    let mut by_input: HashMap<TransactionOutpoint, CovenantLink> = HashMap::new();

    // Walk selected parents from the virtual tip back to (and including) block_deploy.
    let tip = l1.grpc_client().get_block_dag_info().await.expect("dag info").sink;
    let mut cursor = tip;
    loop {
        let block = l1.grpc_client().get_block(cursor, true).await.expect("get_block");
        for rpc_tx in &block.transactions {
            let tx = match Transaction::try_from(rpc_tx.clone()) {
                Ok(tx) => tx,
                Err(_) => continue,
            };
            // A settlement of this covenant binds output 0 to it and ends input 0 with the
            // settlement tail; `settlement_info` returns Some only for those.
            if tx.settlement_info(covenant_id, cursor).is_none() {
                continue;
            }
            let covenant_input = tx.inputs.first().expect("settlement input").previous_outpoint;
            let change_spks =
                tx.outputs.iter().map(|o| o.script_public_key.clone()).collect::<Vec<_>>();
            by_input.insert(
                covenant_input,
                CovenantLink { tx_id: tx.id(), covenant_input, change_spks },
            );
        }
        if cursor == block_deploy {
            break;
        }
        let selected_parent =
            block.verbose_data.as_ref().expect("verbose data").selected_parent_hash;
        cursor = selected_parent;
    }

    // Thread the chain from the bootstrap outpoint forward.
    let mut chain = Vec::new();
    let mut next_input = bootstrap_outpoint;
    while let Some(link) = by_input.remove(&next_input) {
        next_input = TransactionOutpoint::new(link.tx_id, 0);
        chain.push(link);
    }
    chain
}

/// Connects a Borsh wRPC client to `url`, mirroring `main.rs`'s client construction.
async fn connect_wrpc(url: &str, network_id: NetworkId) -> KaspaRpcClient {
    let client =
        KaspaRpcClient::new_with_args(WrpcEncoding::Borsh, Some(url), None, Some(network_id), None)
            .expect("create wRPC client");
    client
        .connect(Some(ConnectOptions {
            block_async_connect: true,
            connect_timeout: Some(Duration::from_millis(10_000)),
            ..Default::default()
        }))
        .await
        .expect("connect to node wRPC");
    client
}
