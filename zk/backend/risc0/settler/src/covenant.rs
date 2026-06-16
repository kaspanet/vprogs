use kaspa_consensus_core::{
    mass::units::ComputeBudget,
    tx::{ScriptPublicKey, Transaction, TransactionOutpoint},
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
    SettlementInput, SuccinctPins, build_redeem_script, redeem_script_len,
};

/// The live on-chain covenant: identity, committed state, and the UTXO that carries it. Advanced by
/// each confirmed settlement.
#[derive(Clone)]
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

/// A production settlement built for one proven bundle: the transaction ready for fee/mass
/// finalization, its minimal sufficient covenant compute budget, and how to advance the covenant
/// once the finalized tx confirms. Built by [`build_settlement`]; the caller owns submission and
/// confirmation.
pub struct BuiltSettlement {
    /// The settlement transaction, ready to fund a fee and finalize mass before submission.
    pub transaction: Transaction,
    /// Minimal sufficient covenant-input compute budget, sized off the artifact's seq-commit
    /// anchor (an oversized budget inflates the tx's compute mass past the per-tx limit and
    /// the node rejects it).
    pub compute_budget: ComputeBudget,
    /// The covenant state this settlement advances to, pending the finalized tx's txid and
    /// confirmation DAA score.
    pub advance: CovenantAdvance,
}

/// The covenant state a built settlement advances to, deferred until the settlement's finalized tx
/// id and the DAA score of its continuation UTXO are known (those depend on caller-specific fee
/// funding and confirmation, which [`build_settlement`] does not do).
pub struct CovenantAdvance {
    covenant_id: Hash,
    new_state: [u8; 32],
    new_lane_tip: Hash,
    continuation_spk: ScriptPublicKey,
    value: u64,
}

impl CovenantAdvance {
    /// P2SH SPK of the continuation output: the next covenant UTXO's script, used to locate it on
    /// chain while confirming.
    pub fn continuation_spk(&self) -> &ScriptPublicKey {
        &self.continuation_spk
    }

    /// The covenant advanced past this settlement, given the finalized tx's `txid` (the
    /// continuation UTXO is its output 0) and the DAA score that UTXO confirmed at. Pass
    /// `daa_score` 0 when the confirmation score is not yet known (stamp it on confirmation).
    pub fn apply(self, txid: Hash, daa_score: u64) -> CovenantState {
        CovenantState {
            covenant_id: self.covenant_id,
            state: self.new_state,
            lane_tip: self.new_lane_tip,
            outpoint: TransactionOutpoint::new(txid, 0),
            spk: self.continuation_spk,
            value: self.value,
            daa_score,
        }
    }
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

/// The production redeem pins for this covenant: the batch + transaction guest image ids, the lane
/// key, and the default permission-output value. Stable across the run.
pub(crate) fn redeem_pins<'a>(backend: &'a Backend, lane_key: &'a Hash) -> RedeemPins<'a> {
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
pub(crate) struct AnchorSeqCommit {
    pub(crate) block: Hash,
    pub(crate) seq_commit: Hash,
}

impl SeqCommitAccessor for AnchorSeqCommit {
    fn is_chain_ancestor_from_pov(&self, block_hash: Hash) -> Option<bool> {
        (block_hash == self.block).then_some(true)
    }

    fn seq_commitment_within_depth(&self, block_hash: Hash) -> Option<Hash> {
        (block_hash == self.block).then_some(self.seq_commit)
    }
}
