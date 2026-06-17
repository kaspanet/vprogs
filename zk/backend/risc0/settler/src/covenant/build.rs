use kaspa_consensus_core::{
    mass::units::ComputeBudget,
    tx::{ScriptPublicKey, TransactionOutpoint},
};
use kaspa_hashes::Hash;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_txscript::standard::pay_to_script_hash_script;
use vprogs_core_smt::EMPTY_HASH;
use vprogs_l1_wallet::Wallet;
use vprogs_zk_aggregate_prover::SettlementArtifact;
use vprogs_zk_backend_risc0_api::{Backend, OwnedSuccinctWitness, Receipt};
use vprogs_zk_backend_risc0_covenant::{
    CommonPins, DEFAULT_PERMISSION_OUTPUT_VALUE, RedeemPins, SeqCommitAccessor, Settlement,
    SettlementDevInput, SettlementInput, SuccinctPins, build_dev_redeem_script,
    build_redeem_script, dev_redeem_script_len, redeem_script_len,
};

/// Covenant-input compute budget for a dev settlement: the dev redeem has no `OpZkPrecompile`, so a
/// small fixed budget covers its hash + concat + equality ops. Production settlements size their
/// budget off the receipt instead.
pub const DEV_COVENANT_BUDGET: ComputeBudget = ComputeBudget(100);

use super::{BuiltSettlement, CovenantAdvance, CovenantState};

/// Bootstraps a fresh production-pins covenant (terminates in `OpZkPrecompile`, so the first
/// settlement's reconstructed prev redeem matches this UTXO's SPK) bound to `lane_key`, locking
/// `value` sompi. Submits it and returns the initial [`CovenantState`] plus the bootstrap txid; the
/// state's `daa_score` is 0 until [`run`](crate::run) confirms the UTXO. The covenant's initial
/// state is the empty SMT, matching a fresh prover store, so the first proved bundle chains from
/// here.
pub async fn bootstrap_real_covenant<C: RpcApi + ?Sized>(
    wallet: &Wallet<'_, C>,
    backend: &Backend,
    lane_key: Hash,
    value: u64,
) -> (CovenantState, Hash) {
    let (redeem, spk) = bootstrap_redeem(backend, &lane_key);

    let (tx, covenant_id) = wallet.build_covenant_bootstrap_transaction(&redeem, value).await;
    let txid = wallet.submit_transaction(&tx).await.expect("bootstrap submission failed");

    let covenant = CovenantState {
        covenant_id,
        state: EMPTY_HASH,
        lane_tip: Hash::default(),
        outpoint: TransactionOutpoint::new(txid, 0),
        spk,
        value,
        daa_score: 0,
    };
    (covenant, txid)
}

/// The production redeem script and its P2SH `ScriptPublicKey` for a fresh covenant: empty initial
/// state, empty lane tip, pins from `backend` / `lane_key`. The first settlement's reconstructed
/// prev redeem must match this UTXO's SPK, so bootstrap and settlement build it identically; this
/// is the single place that construction lives. Returns `(redeem_script, p2sh_spk)`.
pub fn bootstrap_redeem(backend: &Backend, lane_key: &Hash) -> (Vec<u8>, ScriptPublicKey) {
    let state = EMPTY_HASH;
    let lane_tip = Hash::default();
    let pins = redeem_pins(backend, lane_key);
    let redeem_len = redeem_script_len(&state, &pins);
    let redeem = build_redeem_script(&state, &lane_tip, redeem_len, &pins);
    let spk = pay_to_script_hash_script(&redeem);
    (redeem, spk)
}

/// Bootstraps a fresh dev-pins covenant (the [`build_dev_redeem_script`] variant: chain-anchored
/// seq-commit check, no `OpZkPrecompile`) bound to `lane_key`, locking `value` sompi. Submits it
/// and returns the initial [`CovenantState`] plus the bootstrap txid. The dev counterpart of
/// [`bootstrap_real_covenant`], for `RISC0_DEV_MODE` runs where the prover emits stub receipts the
/// production precompile would reject. Like the real bootstrap, the initial state is the empty SMT,
/// so the first proved bundle chains from here; the returned state's `daa_score` is 0 until the
/// UTXO confirms.
pub async fn bootstrap_dev_covenant<C: RpcApi + ?Sized>(
    wallet: &Wallet<'_, C>,
    lane_key: Hash,
    value: u64,
) -> (CovenantState, Hash) {
    let (redeem, spk) = dev_bootstrap_redeem(&lane_key);

    let (tx, covenant_id) = wallet.build_covenant_bootstrap_transaction(&redeem, value).await;
    let txid = wallet.submit_transaction(&tx).await.expect("dev bootstrap submission failed");

    let covenant = CovenantState {
        covenant_id,
        state: EMPTY_HASH,
        lane_tip: Hash::default(),
        outpoint: TransactionOutpoint::new(txid, 0),
        spk,
        value,
        daa_score: 0,
    };
    (covenant, txid)
}

/// The dev redeem script and its P2SH `ScriptPublicKey` for a fresh covenant: empty initial state,
/// empty lane tip, `lane_key` pinned. The dev counterpart of [`bootstrap_redeem`]; the first dev
/// settlement's reconstructed prev redeem must match this UTXO's SPK, so bootstrap and settlement
/// build it identically. Returns `(redeem_script, p2sh_spk)`.
pub fn dev_bootstrap_redeem(lane_key: &Hash) -> (Vec<u8>, ScriptPublicKey) {
    let state = EMPTY_HASH;
    let lane_tip = Hash::default();
    let redeem_len = dev_redeem_script_len(&state, lane_key);
    let redeem = build_dev_redeem_script(&state, &lane_tip, lane_key, redeem_len);
    let spk = pay_to_script_hash_script(&redeem);
    (redeem, spk)
}

/// Builds the production settlement for one proven bundle against the live covenant `cov`.
///
/// Asserts the artifact's bounds chain from `cov`; the on-chain script enforces the same, but a
/// local mismatch means the prover was seeded with the wrong covenant, so fail loudly before paying
/// to submit. Builds the [`Settlement`], sizes its covenant compute budget off the artifact's
/// seq-commit anchor, and returns both with a [`CovenantAdvance`] the caller applies once the
/// finalized tx confirms. Pure: the caller funds the fee, submits, and confirms.
pub fn build_settlement(
    backend: &Backend,
    lane_key: &Hash,
    cov: &CovenantState,
    artifact: &SettlementArtifact<Receipt>,
) -> BuiltSettlement {
    assert_eq!(
        artifact.prev_state, cov.state,
        "settlement prev_state must chain from the live covenant state",
    );
    assert_eq!(
        artifact.prev_lane_tip, cov.lane_tip,
        "settlement prev_lane_tip must match the spent covenant's redeem prefix",
    );
    assert_eq!(
        Hash::from_bytes(artifact.covenant_id),
        cov.covenant_id,
        "settlement covenant_id must match the live covenant",
    );

    let owned_witness = OwnedSuccinctWitness::from_receipt(&artifact.receipt);
    let settlement = Settlement::build(&SettlementInput {
        covenant_id: cov.covenant_id,
        pins: redeem_pins(backend, lane_key),
        prev_state: &artifact.prev_state,
        prev_lane_tip: &artifact.prev_lane_tip,
        new_state: &artifact.new_state,
        new_lane_tip: &artifact.new_lane_tip,
        block_prove_to: artifact.block_prove_to,
        prev_outpoint: cov.outpoint,
        value: cov.value,
        witness: owned_witness.as_witness(),
        permission_spk_hash: &artifact.permission_spk_hash,
    });
    let continuation_spk = pay_to_script_hash_script(&settlement.next_redeem);

    // The script engine anchors `new_seq_commit` to `block_prove_to`; feed it the bundle's value so
    // the budget covers exactly the units the covenant input consumes.
    let accessor =
        AnchorSeqCommit { block: artifact.block_prove_to, seq_commit: artifact.new_seq_commit };
    let compute_budget = settlement.covenant_compute_budget(cov.covenant_id, &accessor);

    BuiltSettlement {
        transaction: settlement.transaction,
        compute_budget,
        advance: CovenantAdvance {
            covenant_id: cov.covenant_id,
            new_state: artifact.new_state,
            new_lane_tip: artifact.new_lane_tip,
            continuation_spk,
            value: cov.value,
        },
    }
}

/// Builds a dev settlement for one proven bundle against the live dev covenant `cov`.
///
/// The dev counterpart of [`build_settlement`]: same authoritative bounds from `artifact` and the
/// same chaining asserts, but the [`build_dev_redeem_script`] variant (no `OpZkPrecompile`; the
/// chain anchors the claimed seq commit instead). The journal's `new_seq_commit` is fed through as
/// the `claimed_seq_commit` the dev script `OpEqualVerify`s against `OpChainblockSeqCommit`, so the
/// guest-committed value and the chain's value must agree exactly. The covenant compute budget is
/// the fixed [`DEV_COVENANT_BUDGET`] (no precompile to size for).
///
/// The dev redeem only handles the single-output (no-exit) path, so a bundle carrying L2→L1 exits
/// (non-zero `permission_spk_hash`) is rejected here rather than silently dropping them.
pub fn build_dev_settlement(
    lane_key: &Hash,
    cov: &CovenantState,
    artifact: &SettlementArtifact<Receipt>,
) -> BuiltSettlement {
    assert_eq!(
        artifact.prev_state, cov.state,
        "dev settlement prev_state must chain from the live covenant state",
    );
    assert_eq!(
        artifact.prev_lane_tip, cov.lane_tip,
        "dev settlement prev_lane_tip must match the spent covenant's redeem prefix",
    );
    assert_eq!(
        Hash::from_bytes(artifact.covenant_id),
        cov.covenant_id,
        "dev settlement covenant_id must match the live covenant",
    );
    assert_eq!(
        artifact.permission_spk_hash, [0u8; 32],
        "dev settlement does not support permission exits (the dev redeem pins output count to 1)",
    );

    let settlement = Settlement::build_dev(&SettlementDevInput {
        covenant_id: cov.covenant_id,
        prev_state: &artifact.prev_state,
        prev_lane_tip: &artifact.prev_lane_tip,
        lane_key,
        new_state: &artifact.new_state,
        new_lane_tip: &artifact.new_lane_tip,
        block_prove_to: artifact.block_prove_to,
        claimed_seq_commit: artifact.new_seq_commit,
        prev_outpoint: cov.outpoint,
        value: cov.value,
    });
    let continuation_spk = pay_to_script_hash_script(&settlement.next_redeem);

    BuiltSettlement {
        transaction: settlement.transaction,
        compute_budget: DEV_COVENANT_BUDGET,
        advance: CovenantAdvance {
            covenant_id: cov.covenant_id,
            new_state: artifact.new_state,
            new_lane_tip: artifact.new_lane_tip,
            continuation_spk,
            value: cov.value,
        },
    }
}

/// The production redeem pins for this covenant: the batch + transaction guest image ids, the lane
/// key, and the default permission-output value. Stable across the run.
fn redeem_pins<'a>(backend: &'a Backend, lane_key: &'a Hash) -> RedeemPins<'a> {
    RedeemPins::Succinct(SuccinctPins {
        common: CommonPins {
            program_id: &backend.aggregator.id,
            tx_image_id: &backend.transaction_processor.id,
            batch_image_id: &backend.batch_processor.id,
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
