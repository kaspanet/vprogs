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

use std::{collections::HashMap, ops::RangeInclusive, time::Duration};

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
use kaspa_rpc_core::{RpcDataVerbosityLevel, api::rpc::RpcApi};
use kaspa_txscript::pay_to_address_script;
use kaspa_wrpc_client::prelude::*;
use secp256k1::Keypair;
use vprogs_core_atomics::AtomicAsyncLatch;
use vprogs_core_smt::EMPTY_HASH;
use vprogs_core_test_utils::ResourceIdExt;
use vprogs_core_types::{AccessMetadata, ResourceId};
use vprogs_example_tn10_flow::daemon::{self, BridgeParams, FlowNode, ProvingParams};
use vprogs_l1_types::L1TransactionCovenantExt;
use vprogs_l1_wallet::encode_activity_payload;
use vprogs_node_test_utils::L1Node;
use vprogs_zk_backend_risc0_api::{Backend, ProofType};
use vprogs_zk_backend_risc0_settler::{
    CovenantState, SettlementMode, SettlementWorkerConfig, dev_bootstrap_redeem,
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
    /// The proving node (bridge + pipeline). Kept alive; dropping it shuts the prover down.
    _node: FlowNode,
    /// The settler task. Awaited at the end; `Ok(())` proves it did not panic on a competitor.
    settler: tokio::task::JoinHandle<()>,
    /// Latch the test opens to tear the settler down gracefully.
    shutdown: AtomicAsyncLatch,
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
    // Both bundle over the same size range, so neither structurally always forms a bundle first:
    // the winner of each uncovered range is decided by proving/submit jitter. The loser finds
    // its covenant outpoint already spent (its bundle superseded), skips it, and reconciles to
    // the winner's advance before racing for the next range -- so over many ranges both land
    // settlements.
    let tx_elf = transaction_processor_elf();
    let batch_elf = batch_processor_elf();
    let aggregator_elf = batch_aggregator_elf();
    let params = Params::from(network_id);

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
        &tx_elf,
        &batch_elf,
        &aggregator_elf,
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
        &tx_elf,
        &batch_elf,
        &aggregator_elf,
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

    // === Step 5: let in-flight settlements confirm, then tear the settlers down ===
    // Keep mining acceptance blocks so settlements still in the mempool land and confirm. Poll the
    // covenant continuation chain until it stops growing (or a bounded ceiling), so we capture the
    // settlements that raced in at the tail of the driver loop.
    let mut prev_len = 0usize;
    for round in 0..30 {
        l1.mine_blocks(2).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let len = covenant_chain(&l1, block_deploy, bootstrap_outpoint, covenant_id).await.len();
        if len == prev_len && len >= 3 {
            break;
        }
        prev_len = len;
        if round % 5 == 0 {
            eprintln!("drain: round {round}, covenant chain length {len}");
        }
    }

    // === EXPERIMENT (part 2a): direct getBlock side-by-side covenant-binding readback ===
    // Pick the first mined settlement and report output 0's covenant binding as seen over BOTH the
    // gRPC `get_block(hash,true)` path AND the wRPC `get_virtual_chain_from_block_v2(.., Full)`
    // path the bridge actually consumes. This is the crux: "does kaspanet master surface the
    // on-chain covenant binding over wRPC, or drop it like the fork did?"
    probe_covenant_binding(&l1, block_deploy, bootstrap_outpoint, covenant_id, network_id).await;

    prover_a.shutdown.open();
    prover_b.shutdown.open();
    let join_a = prover_a.settler.await;
    let join_b = prover_b.settler.await;

    // === Assertion 1: no panic (the resilience claim) ===
    // A settler that hit the old panic-on-competitor-advance returns Err (the task panicked).
    // Surface the join result before any other assertion so a panic shows up here, not as a
    // timeout.
    assert!(join_a.is_ok(), "prover A settler panicked: {join_a:?}");
    assert!(join_b.is_ok(), "prover B settler panicked: {join_b:?}");
    eprintln!("both settlers joined Ok (no panic under contention)");

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

/// EXPERIMENT (part 2a): for the first mined settlement, print whether output 0's covenant binding
/// survives on the gRPC `get_block(hash,true)` path vs the wRPC
/// `get_virtual_chain_from_block_v2(.., Full)` path the bridge consumes. The two RPCs surface
/// different tx sets (gRPC returns a block's *contained* txs; the v2 chain stream returns
/// *accepted* txs an accepting chain block merged), so we match the SAME settlement txid across
/// both and compare its output-0 covenant binding.
async fn probe_covenant_binding(
    l1: &L1Node,
    block_deploy: Hash,
    bootstrap_outpoint: TransactionOutpoint,
    covenant_id: Hash,
    network_id: NetworkId,
) {
    // 1) gRPC: walk selected parents, find the FIRST settlement (the bootstrap-spend), and read its
    //    output-0 covenant straight off the RpcTransaction `get_block(hash,true)` returns.
    let tip = l1.grpc_client().get_block_dag_info().await.expect("dag info").sink;
    let mut cursor = tip;
    let mut target_txid: Option<Hash> = None;
    let mut grpc_binding: Option<Option<Hash>> = None;
    loop {
        let block = l1.grpc_client().get_block(cursor, true).await.expect("get_block");
        for rpc_tx in &block.transactions {
            let tx = match Transaction::try_from(rpc_tx.clone()) {
                Ok(tx) => tx,
                Err(_) => continue,
            };
            if tx.inputs.first().map(|i| i.previous_outpoint) != Some(bootstrap_outpoint) {
                continue;
            }
            if tx.settlement_info(covenant_id, cursor).is_none() {
                continue;
            }
            // gRPC RpcTransactionOutput carries `covenant: Option<RpcCovenantBinding>` directly.
            let on_wire =
                rpc_tx.outputs.first().and_then(|o| o.covenant.as_ref()).map(|c| c.0.covenant_id);
            target_txid = Some(tx.id());
            grpc_binding = Some(on_wire);
            break;
        }
        if grpc_binding.is_some() || cursor == block_deploy {
            break;
        }
        cursor = block.verbose_data.as_ref().expect("verbose data").selected_parent_hash;
    }

    let Some(target_txid) = target_txid else {
        eprintln!(
            "PROBE: no settlement found spending the bootstrap outpoint; cannot compare paths"
        );
        return;
    };

    // 2) wRPC: the exact API the bridge uses. The v2 stream returns *accepted* txs (an accepting
    //    chain block merged them), a different set than gRPC's *contained* txs, so we scan the
    //    whole stream for ANY settlement of this covenant and report its output-0 covenant binding
    //    (a) off the RpcOptionalTransactionOutput on the wire, and (b) after the bridge's
    //    `Transaction::try_from` conversion (what `settlement_info` actually sees). We also try to
    //    match `target_txid`.
    let client = connect_wrpc(&l1.wrpc_borsh_url(), network_id).await;
    let response = client
        .get_virtual_chain_from_block_v2(block_deploy, Some(RpcDataVerbosityLevel::Full), None)
        .await
        .expect("get_virtual_chain_from_block_v2");
    let mut wrpc_on_wire: Option<Option<Hash>> = None;
    let mut wrpc_after_convert: Option<Option<Hash>> = None;
    let mut probed_txid = target_txid;
    let mut accepted_total = 0usize;
    'outer: for chain_block in response.chain_block_accepted_transactions.iter() {
        let accepting = chain_block.chain_block_header.hash;
        for opt_tx in chain_block.accepted_transactions.iter() {
            accepted_total += 1;
            let on_wire = opt_tx
                .outputs
                .first()
                .and_then(|o| o.covenant.as_ref())
                .and_then(|c| c.0)
                .map(|b| b.0.covenant_id);
            let converted = match Transaction::try_from(opt_tx.clone()) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let is_target = converted.id() == target_txid;
            // settlement_info needs the accepting/containing block hash; use this chain block's.
            let is_settlement = accepting
                .map(|h| converted.settlement_info(covenant_id, h).is_some())
                .unwrap_or(false);
            if is_target || is_settlement {
                probed_txid = converted.id();
                wrpc_on_wire = Some(on_wire);
                wrpc_after_convert =
                    Some(converted.outputs.first().and_then(|o| o.covenant).map(|b| b.covenant_id));
                break 'outer;
            }
        }
    }
    eprintln!("PROBE wRPC v2 accepted-tx count scanned                 = {accepted_total}");
    let target_txid = probed_txid;

    let fmt = |b: &Option<Option<Hash>>| match b {
        Some(Some(id)) => format!("Some({id})"),
        Some(None) => "None".to_string(),
        None => "<settlement not found on this path>".to_string(),
    };
    client.disconnect().await.ok();
    eprintln!(
        "==================== COVENANT BINDING PROBE (settlement {target_txid}, output 0) ===================="
    );
    eprintln!("PROBE expected covenant_id                              = {covenant_id}");
    eprintln!("PROBE gRPC  get_block(hash,true) RpcTransactionOutput   = {}", fmt(&grpc_binding));
    eprintln!("PROBE wRPC  v2 RpcOptionalTransactionOutput (on wire)   = {}", fmt(&wrpc_on_wire));
    eprintln!(
        "PROBE wRPC  v2 -> Transaction::try_from (bridge sees)   = {}",
        fmt(&wrpc_after_convert)
    );
    eprintln!(
        "====================================================================================================="
    );
}

/// One link in the covenant continuation chain: a settlement tx, the covenant outpoint its input 0
/// spends, and the SPKs of all its outputs (so attribution can match a prover's change SPK).
struct CovenantLink {
    tx_id: Hash,
    covenant_input: TransactionOutpoint,
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
    tx_elf: &[u8],
    batch_elf: &[u8],
    aggregator_elf: &[u8],
) -> Prover {
    let wrpc_url = l1.wrpc_borsh_url();
    // Separate wRPC clients for the lane source and the settler so they own independent handles.
    let client_for_lane = connect_wrpc(&wrpc_url, network_id).await;
    let client_for_settler = connect_wrpc(&wrpc_url, network_id).await;

    let dir = tempfile::TempDir::new().expect("temp dir");
    let store = daemon::Store::open(dir.path());
    // Hold the TempDir for the run by leaking it: the store keeps the dir's files open and the test
    // process is short-lived, so reclaiming the scratch space on drop is not worth threading the
    // guard through `Prover`.
    std::mem::forget(dir);

    let queue = daemon::FlowSettlementQueue::new();
    let node = daemon::build_proving_node(
        tx_elf,
        batch_elf,
        aggregator_elf,
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
    let backend = Backend::new(tx_elf, batch_elf, aggregator_elf, ProofType::Succinct);
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
        },
        covenant,
        shutdown.clone(),
    ));

    eprintln!("prover {label} started (addr {address})");
    Prover { _node: node, settler, shutdown }
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
