use kaspa_consensus_core::{
    mass::units::ComputeBudget,
    tx::{ScriptPublicKey, TransactionOutpoint},
};
use kaspa_hashes::Hash;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_txscript::standard::pay_to_script_hash_script;
use vprogs_core_smt::EMPTY_HASH;
use vprogs_l1_types::SettlementInfo;
use vprogs_l1_wallet::Wallet;
use vprogs_zk_aggregate_prover::SettlementArtifact;
use vprogs_zk_backend_risc0_api::{Backend, OwnedSuccinctWitness, Receipt};
use vprogs_zk_backend_risc0_covenant::{
    CommonPins, DEFAULT_PERMISSION_OUTPUT_VALUE, RedeemPins, SeqCommitAccessor, Settlement,
    SettlementDevInput, SettlementInput, SuccinctPins, build_dev_redeem_script,
    build_redeem_script, dev_redeem_script_len, redeem_script_len,
};

/// Covenant-input compute budget for a dev settlement.
pub const DEV_COVENANT_BUDGET: ComputeBudget = ComputeBudget(100);

use super::{BuiltSettlement, CovenantAdvance, CovenantState};
use crate::worker::SettlementMode;

/// Bootstraps a fresh production-pins covenant bound to `lane_key` and returns its initial state
/// plus bootstrap txid.
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

/// Returns the production redeem script and P2SH `ScriptPublicKey` for a fresh covenant.
pub fn bootstrap_redeem(backend: &Backend, lane_key: &Hash) -> (Vec<u8>, ScriptPublicKey) {
    let state = EMPTY_HASH;
    let lane_tip = Hash::default();
    let pins = redeem_pins(backend, lane_key);
    let redeem_len = redeem_script_len(&state, &pins);
    let redeem = build_redeem_script(&state, &lane_tip, redeem_len, &pins);
    let spk = pay_to_script_hash_script(&redeem);
    (redeem, spk)
}

/// Bootstraps a fresh dev-pins covenant bound to `lane_key` and returns its initial state plus
/// bootstrap txid.
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

/// Returns the dev redeem script and P2SH `ScriptPublicKey` for a fresh covenant.
pub fn dev_bootstrap_redeem(lane_key: &Hash) -> (Vec<u8>, ScriptPublicKey) {
    let state = EMPTY_HASH;
    let lane_tip = Hash::default();
    let redeem_len = dev_redeem_script_len(&state, lane_key, DEFAULT_PERMISSION_OUTPUT_VALUE);
    let redeem = build_dev_redeem_script(
        &state,
        &lane_tip,
        lane_key,
        redeem_len,
        DEFAULT_PERMISSION_OUTPUT_VALUE,
    );
    let spk = pay_to_script_hash_script(&redeem);
    (redeem, spk)
}

/// Builds the settlement for one proven bundle in the configured [`SettlementMode`].
pub fn build_settlement_for_mode(
    mode: SettlementMode,
    backend: &Backend,
    lane_key: &Hash,
    cov: &CovenantState,
    artifact: &SettlementArtifact<Receipt>,
) -> BuiltSettlement {
    match mode {
        SettlementMode::Production => build_settlement(backend, lane_key, cov, artifact),
        SettlementMode::Dev => build_dev_settlement(lane_key, cov, artifact),
    }
}

/// Builds the production settlement for one proven bundle against the live covenant `cov`.
///
/// Panics if the artifact does not chain from `cov`.
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

    // The rebuilt redeem must reproduce the spent UTXO's actual SPK. The redeem pins the
    // bootstrap guest-ELF image ids, and the node's P2SH entry check compares against the chain
    // SPK, so a prover whose guests were rebuilt since the covenant's bootstrap builds a redeem
    // with different bytes here: every chain check above still passes, the local pre-submit
    // engine run still passes (it verifies against this same rebuilt redeem), and only the node
    // rejects the settlement with an opaque `false stack entry at end of script execution`.
    // Fail here instead, naming the drift.
    let pins = redeem_pins(backend, lane_key);
    let redeem_len = redeem_script_len(&artifact.prev_state, &pins);
    let prev_redeem =
        build_redeem_script(&artifact.prev_state, &artifact.prev_lane_tip, redeem_len, &pins);
    assert_eq!(
        pay_to_script_hash_script(&prev_redeem),
        cov.spk,
        "locally rebuilt redeem does not reproduce the covenant UTXO's SPK: the redeem pins \
         (guest ELF image ids, lane key, permission value) differ from the ones this covenant \
         was bootstrapped with, so the node would reject the settlement at its P2SH hash check; \
         rejoin with the covenant's original guest ELFs or bootstrap a fresh covenant",
    );

    let owned_witness =
        OwnedSuccinctWitness::from_receipt(&artifact.receipt, artifact.deposit_spk_hash);
    let settlement = Settlement::build(&SettlementInput {
        covenant_id: cov.covenant_id,
        pins,
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
/// Panics if the artifact does not chain from `cov`.
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
    // Same guard as the production builder: the rebuilt dev redeem must reproduce the spent
    // UTXO's actual SPK, else the node's P2SH entry check rejects the settlement opaquely.
    let dev_len =
        dev_redeem_script_len(&artifact.prev_state, lane_key, DEFAULT_PERMISSION_OUTPUT_VALUE);
    let prev_redeem = build_dev_redeem_script(
        &artifact.prev_state,
        &artifact.prev_lane_tip,
        lane_key,
        dev_len,
        DEFAULT_PERMISSION_OUTPUT_VALUE,
    );
    assert_eq!(
        pay_to_script_hash_script(&prev_redeem),
        cov.spk,
        "locally rebuilt dev redeem does not reproduce the covenant UTXO's SPK: the redeem pins \
         (lane key, permission value) differ from the ones this covenant was bootstrapped with, \
         so the node would reject the settlement at its P2SH hash check",
    );
    let settlement = Settlement::build_dev(&SettlementDevInput {
        deposit_spk_hash: &artifact.deposit_spk_hash,
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
        permission_spk_hash: &artifact.permission_spk_hash,
        permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
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

/// Rebuilds the live [`CovenantState`] from an external settlement `s` that advanced the covenant.
///
/// The continuation SPK comes from the observed transaction itself (`s.continuation_spk_hash`),
/// never from a local-pin rebuild: the redeem script pins the bootstrap guest-ELF image ids, and
/// a catch-up prover whose guests were rebuilt would otherwise adopt an SPK its own settlements
/// can never reproduce, deferring the mismatch to an opaque node rejection.
pub fn covenant_from_settlement(cov: &CovenantState, s: &SettlementInfo) -> CovenantState {
    CovenantState {
        covenant_id: cov.covenant_id,
        state: s.new_state,
        lane_tip: s.new_lane_tip,
        outpoint: TransactionOutpoint::new(s.tx_id, 0),
        spk: p2sh_spk_from_hash(&s.continuation_spk_hash),
        value: cov.value,
        daa_score: s.daa_score.get(),
    }
}

/// Rebuilds the P2SH `ScriptPublicKey` a settlement's continuation output carried, from the
/// redeem-script hash the bridge captured. Layout matches `pay_to_script_hash_script`:
/// `OpBlake2b | OpData32 | hash(32) | OpEqual`.
fn p2sh_spk_from_hash(hash: &[u8; 32]) -> ScriptPublicKey {
    use kaspa_txscript::opcodes::codes::{OpBlake2b, OpData32, OpEqual};
    let mut script = Vec::with_capacity(35);
    script.extend_from_slice(&[OpBlake2b, OpData32]);
    script.extend_from_slice(hash);
    script.push(OpEqual);
    ScriptPublicKey::new(0, script.into())
}

/// Returns the production redeem pins for this covenant.
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

/// Single-block [`SeqCommitAccessor`] for sizing the covenant input's compute budget off chain.
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
