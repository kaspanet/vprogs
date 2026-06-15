use kaspa_consensus_core::tx::{ScriptPublicKey, TransactionOutpoint};
use kaspa_hashes::Hash;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_txscript::standard::pay_to_script_hash_script;
use vprogs_core_smt::EMPTY_HASH;
use vprogs_l1_wallet::Wallet;
use vprogs_zk_backend_risc0_api::Backend;
use vprogs_zk_backend_risc0_covenant::{
    CommonPins, DEFAULT_PERMISSION_OUTPUT_VALUE, RedeemPins, SeqCommitAccessor, SuccinctPins,
    build_redeem_script, redeem_script_len,
};

/// The live on-chain covenant: identity, committed state, and the UTXO that carries it. Advanced by
/// each confirmed settlement.
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
