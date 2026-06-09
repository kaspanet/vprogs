//! Settles proven bundles against the remote node.
//!
//! The framework [`Node`](vprogs_node_framework::Node) follows the chain and proves each block's
//! batch, but it cannot settle: the bundle receipt lives only on the [`ScheduledBatch`] handle the
//! worker hands us through the batch sink. This task owns those handles, and once a bundle's
//! receipt publishes it builds a production [`Settlement::build`] (real receipt → on-chain
//! `OpZkPrecompile`) that spends the live covenant and chains to the journal's `new_state` /
//! `new_lane_tip`, exactly as `sim::driver::settle_real` and `settlement_l1_e2e` do — submitting it
//! over the same wRPC client the rest of the binary uses.
//!
//! Settlements are serialized: one in flight, the next built only after the previous one's
//! continuation UTXO is confirmed on chain (so it can be spent). This mirrors the simulation's
//! single-in-flight covenant and is **single-miner / low-reorg only**: a reorg that orphans a block
//! whose batch is mid-prove desyncs our `unproved` queue from the prover's stream. The worker
//! forwards rollbacks so we can drop orphaned batches, but cancelling an in-flight bundle proof is
//! still a framework-side gap, so run this against a clean/fork node.

use std::{collections::VecDeque, time::Duration};

use kaspa_addresses::Prefix;
use kaspa_consensus_core::{
    config::params::Params,
    tx::{ScriptPublicKey, TransactionOutpoint, UtxoEntry},
};
use kaspa_hashes::Hash;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_txscript::standard::{extract_script_pub_key_address, pay_to_script_hash_script};
use kaspa_wrpc_client::prelude::KaspaRpcClient;
use secp256k1::Keypair;
use tokio::sync::mpsc::UnboundedReceiver;
use vprogs_core_codec::Reader;
use vprogs_core_smt::EMPTY_HASH;
use vprogs_l1_wallet::Wallet;
use vprogs_node_framework::BatchEvent;
use vprogs_scheduling_scheduler::ScheduledBatch;
use vprogs_zk_abi::batch_processor::StateTransition;
// `Backend as _` brings the batch-prover `Backend` trait into scope for
// `Backend::journal_bytes`.
use vprogs_zk_backend_risc0_api::{Backend, OwnedSuccinctWitness, ProofType};
use vprogs_zk_backend_risc0_covenant::{
    CommonPins, DEFAULT_PERMISSION_OUTPUT_VALUE, RedeemPins, SeqCommitAccessor, Settlement,
    SettlementInput, SuccinctPins, build_redeem_script, redeem_script_len,
};
use vprogs_zk_batch_prover::Backend as _;

use crate::daemon::{FlowBatchEvent, Store, V};

/// Poll cadence and ceiling for waiting on a covenant UTXO to confirm on chain.
const CONFIRM_POLL_INTERVAL: Duration = Duration::from_secs(1);
const CONFIRM_MAX_POLLS: u32 = 300;

/// The live on-chain covenant: identity, committed state, and the UTXO that carries it. Advanced by
/// each confirmed settlement, exactly like the simulation's `Covenant`.
pub struct CovenantState {
    pub covenant_id: Hash,
    pub state: [u8; 32],
    pub lane_tip: Hash,
    pub outpoint: TransactionOutpoint,
    pub spk: ScriptPublicKey,
    pub value: u64,
    /// DAA score of the carrying UTXO, filled in once it confirms (needed to spend it next).
    pub daa_score: u64,
}

/// Everything the settler needs that isn't carried per-batch.
pub struct SettlerConfig {
    /// wRPC client for funding, submission, and confirmation polling.
    pub client: KaspaRpcClient,
    /// Consensus params (mass calc, network prefix).
    pub params: Params,
    /// Key that funds and signs settlement fees.
    pub keypair: Keypair,
    /// Lane key the guest commits and the covenant SPK pins.
    pub lane_key: Hash,
    /// Batches per bundle / settlement (matches the prover's `bundle_size`).
    pub bundle_size: usize,
    /// Transaction-processor ELF, for the backend's image ids / journal decode.
    pub tx_elf: Vec<u8>,
    /// Batch-processor ELF, for the backend's image ids / journal decode.
    pub batch_elf: Vec<u8>,
}

/// Drives the settlement loop until the batch sink closes (node shutdown). Accumulates the
/// scheduled batches the framework forwards, and once a full bundle's receipt publishes, settles it
/// and advances `covenant` (the freshly bootstrapped covenant this run settles against).
pub async fn run(
    mut rx: UnboundedReceiver<FlowBatchEvent>,
    cfg: SettlerConfig,
    covenant: CovenantState,
) {
    let backend = Backend::new(&cfg.tx_elf, &cfg.batch_elf, ProofType::Succinct);
    let bundle_size = cfg.bundle_size.max(1);
    let mut unproved: VecDeque<ScheduledBatch<Store, V>> = VecDeque::new();
    let mut cov = covenant;

    // Confirm the bootstrap UTXO before chaining, so the first settlement can spend it and we know
    // its DAA score.
    cov.daa_score = confirm_outpoint(&cfg.client, &cfg.params, &cov.spk, cov.outpoint).await;
    log::info!("settler: bootstrap covenant {} confirmed (daa {})", cov.covenant_id, cov.daa_score);

    while let Some(event) = rx.recv().await {
        match event {
            BatchEvent::Scheduled(batch) => unproved.push_back(batch),
            BatchEvent::RolledBack(index) => {
                // Single-miner assumption: drop any orphaned batches above the rollback point. A
                // bundle proof already in flight for an orphaned block is the documented hazard.
                let prev_tip = unproved.back().map(|b| b.checkpoint().index());
                let before = unproved.len();
                unproved.retain(|b| b.checkpoint().index() <= index);
                let dropped = before - unproved.len();
                if dropped > 0 {
                    // `index` is the surviving L2 batch the chain rolled back *to*; the dropped
                    // batches are the orphaned forward progress above it (idx+1..=prev_tip).
                    log::warn!(
                        "settler: reorg rolled the L2 tip back to batch idx={index}; dropped \
                         {dropped} orphaned batches above it ({}..={}), {} still queued",
                        index + 1,
                        prev_tip.unwrap_or(index),
                        unproved.len(),
                    );
                }
            }
        }

        // Settle every full bundle that is ready (each call awaits the bundle's receipt, then
        // consumes `bundle_size` batches whether it settled or was a no-op).
        while unproved.len() >= bundle_size {
            if let Some(next) =
                settle_bundle(&cfg, &backend, &cov, &mut unproved, bundle_size).await
            {
                cov = next;
            }
        }
    }
    log::info!("settler: batch sink closed; stopping");
}

/// Awaits the front bundle's receipt and, if the bundle advanced the L2 state, settles it: builds a
/// production `Settlement::build` from the receipt, submits it, and waits for its continuation UTXO
/// to confirm. Returns the advanced covenant, or `None` for a no-op / empty bundle (still
/// consumed).
async fn settle_bundle(
    cfg: &SettlerConfig,
    backend: &Backend,
    cov: &CovenantState,
    unproved: &mut VecDeque<ScheduledBatch<Store, V>>,
    bundle_size: usize,
) -> Option<CovenantState> {
    // Empty batches auto-open their `artifact_published` latch (the execution-only shortcut), so
    // wait on the last *non-empty* batch: the prover publishes the bundle receipt to every batch,
    // so once it opens, every batch in the bundle has its slot set. (Same reasoning as the sim.)
    let last_nonempty = (0..bundle_size).rev().find(|&i| !unproved[i].txs().is_empty());
    let Some(nonempty_idx) = last_nonempty else {
        // All-empty bundle: no L2 advance, nothing to settle. Drop it so the next bundle can.
        for _ in 0..bundle_size {
            unproved.pop_front();
        }
        return None;
    };
    unproved[nonempty_idx].wait_artifact_published().await;

    let bundle: Vec<_> = (0..bundle_size).map(|_| unproved.pop_front().unwrap()).collect();
    let last = bundle.last().unwrap();
    let block_prove_to = last.checkpoint().metadata().hash;
    let claimed_seq_commit = last.checkpoint().metadata().seq_commit;
    // Read from the batch we waited on, not `last`: a batch after `nonempty_idx` may not have its
    // slot populated yet. The same bundle receipt is on every batch.
    let receipt = (*bundle[nonempty_idx].artifact()).clone();

    let journal = Backend::journal_bytes(&receipt);
    let parsed =
        (&mut &journal[..]).array_as::<StateTransition>("state_transition").expect("journal");

    // A no-op bundle (no lane activity in its blocks) leaves the state unchanged: nothing to
    // settle.
    if parsed.new_state == parsed.prev_state {
        return None;
    }

    // The journal is authoritative for the on-chain script; assert the chain-derived covenant
    // agrees before paying to submit, so a mismatch fails loudly and locally (mirrors the sim's
    // asserts).
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
        pins: redeem_pins(backend, &cfg.lane_key),
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

    // Size the covenant input's committed compute budget from the script units it actually
    // consumes, rather than a fixed guess: the script engine anchors `new_seq_commit` to
    // `block_prove_to`, so feed it the journal's value (which we asserted equals the chain's
    // above). An oversized budget inflates the tx's compute mass past the per-tx limit and the
    // node rejects it; this yields the minimal sufficient value.
    let accessor = AnchorSeqCommit { block: block_prove_to, seq_commit: claimed_seq_commit };
    let budget = settlement.covenant_compute_budget(cov.covenant_id, &accessor);

    let covenant_entry =
        UtxoEntry::new(cov.value, cov.spk.clone(), cov.daa_score, false, Some(cov.covenant_id));
    let wallet = Wallet::new(&cfg.client, &cfg.params, cfg.keypair);
    let tx =
        wallet.prepare_settlement_transaction(settlement.transaction, covenant_entry, budget).await;
    let txid = match wallet.submit_transaction(&tx).await {
        Ok(id) => id,
        // A rejection here is the on-chain script (incl. `OpZkPrecompile`) refusing the settlement:
        // surface it loudly — that is exactly the end-to-end check this path exists to make.
        Err(e) => panic!("settlement submit rejected by node: {e}"),
    };
    log::info!("settler: submitted settlement {txid} (block {block_prove_to})");

    let continuation_outpoint = TransactionOutpoint::new(txid, 0);
    let daa_score =
        confirm_outpoint(&cfg.client, &cfg.params, &continuation_spk, continuation_outpoint).await;
    log::info!("settler: settlement {txid} confirmed (daa {daa_score})");

    Some(CovenantState {
        covenant_id: cov.covenant_id,
        state: new_state,
        lane_tip: new_lane_tip,
        outpoint: continuation_outpoint,
        spk: continuation_spk,
        value: cov.value,
        daa_score,
    })
}

/// Bootstraps a fresh production-pins covenant (terminates in `OpZkPrecompile`, so the first
/// settlement's reconstructed prev redeem matches this UTXO's SPK) bound to `lane_key`, locking
/// `value` sompi. Submits it and returns the initial [`CovenantState`] plus the bootstrap txid; the
/// state's `daa_score` is 0 until [`run`] confirms the UTXO. The covenant's initial state is the
/// empty SMT, matching a fresh prover store, so the first proved bundle chains from here.
pub async fn bootstrap_real_covenant<C: kaspa_rpc_core::api::rpc::RpcApi + ?Sized>(
    wallet: &Wallet<'_, C>,
    backend: &Backend,
    lane_key: Hash,
    value: u64,
) -> (CovenantState, Hash) {
    let state = EMPTY_HASH;
    let lane_tip = Hash::default();
    let pins = redeem_pins(backend, &lane_key);
    let redeem_len = redeem_script_len(&state, &pins);
    let redeem = build_redeem_script(&state, &lane_tip, redeem_len, &pins);
    let spk = pay_to_script_hash_script(&redeem);

    let (tx, covenant_id) = wallet.build_covenant_bootstrap_transaction(&redeem, value).await;
    let txid = wallet.submit_transaction(&tx).await.expect("bootstrap submission failed");

    let covenant = CovenantState {
        covenant_id,
        state,
        lane_tip,
        outpoint: TransactionOutpoint::new(txid, 0),
        spk,
        value,
        daa_score: 0,
    };
    (covenant, txid)
}

/// The production redeem pins for this covenant: the batch + transaction guest image ids, the lane
/// key, and the default permission-output value. Stable across the run.
fn redeem_pins<'a>(backend: &'a Backend, lane_key: &'a Hash) -> RedeemPins<'a> {
    RedeemPins::Succinct(SuccinctPins {
        common: CommonPins {
            program_id: backend.batch_image_id(),
            tx_image_id: backend.transaction_image_id(),
            lane_key,
            permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
        },
    })
}

/// Single-block [`SeqCommitAccessor`] for sizing the covenant input's compute budget off chain:
/// resolves the proven block to the journal's `new_seq_commit` (and treats it as an in-depth chain
/// ancestor). On chain the node answers `OpChainblockSeqCommit` from the real DAG; for the local
/// script-engine dry run we only need the one anchor block the redeem script looks up.
struct AnchorSeqCommit {
    block: Hash,
    seq_commit: Hash,
}

impl SeqCommitAccessor for AnchorSeqCommit {
    fn is_chain_ancestor_from_pov(&self, block_hash: Hash) -> Option<bool> {
        (block_hash == self.block).then_some(true)
    }

    fn seq_commitment_within_depth(&self, block_hash: Hash) -> Option<Hash> {
        (block_hash == self.block).then_some(self.seq_commit)
    }
}

/// Polls the node until `outpoint` appears at `spk`'s P2SH address, returning its block DAA score.
/// Covenant UTXOs are P2SH, so the node's utxoindex tracks them by their script address. Panics on
/// timeout (a settlement that never confirms is a liveness failure worth surfacing).
async fn confirm_outpoint(
    client: &KaspaRpcClient,
    params: &Params,
    spk: &ScriptPublicKey,
    outpoint: TransactionOutpoint,
) -> u64 {
    let prefix = Prefix::from(params.net.network_type());
    let address = extract_script_pub_key_address(spk, prefix).expect("covenant P2SH address");
    for _ in 0..CONFIRM_MAX_POLLS {
        let utxos = client
            .get_utxos_by_addresses(vec![address.clone()])
            .await
            .expect("get_utxos_by_addresses");
        if let Some(entry) =
            utxos.into_iter().find(|e| TransactionOutpoint::from(e.outpoint) == outpoint)
        {
            return entry.utxo_entry.block_daa_score;
        }
        tokio::time::sleep(CONFIRM_POLL_INTERVAL).await;
    }
    panic!("covenant outpoint {outpoint} not confirmed at {address} within timeout");
}
