use kaspa_consensus_core::tx::TransactionOutpoint;
use kaspa_hashes::Hash;

/// Inputs describing a single dev-mode settlement step. Mirrors
/// [`SettlementInput`](super::SettlementInput) but drops `program_id` / `tx_image_id` / `witness`
/// (the dev redeem script has no journal binding and no ZK precompile) and adds
/// `claimed_seq_commit`: the sig-script-supplied seq commitment the dev script will
/// [`OpEqualVerify`] against [`OpChainblockSeqCommit(block_prove_to)`].
///
/// [`OpEqualVerify`]: kaspa_txscript::opcodes::codes::OpEqualVerify
/// [`OpChainblockSeqCommit(block_prove_to)`]: kaspa_txscript::opcodes::codes::OpChainblockSeqCommit
pub struct SettlementDevInput<'a> {
    /// Bundle deposit-address commitment `delegate_entry_spk_hash(covenant_id)`, or `[0; 32]` when
    /// the bundle credited no L1 deposit.
    pub deposit_spk_hash: &'a [u8; 32],
    /// Covenant id carried forward by the continuation output.
    pub covenant_id: Hash,
    /// L2 state root before this batch.
    pub prev_state: &'a [u8; 32],
    /// Lane tip embedded in the covenant UTXO's redeem prefix (carried from the previous
    /// settlement).
    pub prev_lane_tip: &'a Hash,
    /// Lane key the dev covenant settles for; pinned into the redeem prefix to keep dev and prod
    /// layouts size-compatible.
    pub lane_key: &'a Hash,
    /// L2 state root after this batch.
    pub new_state: &'a [u8; 32],
    /// Lane tip after this batch (locks into the continuation UTXO's redeem prefix).
    pub new_lane_tip: &'a Hash,
    /// L1 chain block whose seq commitment the dev script anchors `claimed_seq_commit` to.
    pub block_prove_to: Hash,
    /// Seq commitment the host claims for `block_prove_to`. The dev script enforces this
    /// equals the chain's value via `OpEqualVerify` - any divergence between off-chain and
    /// chain-derived seq commits will fail script execution.
    pub claimed_seq_commit: Hash,
    /// UTXO outpoint of the covenant being spent.
    pub prev_outpoint: TransactionOutpoint,
    /// Value carried on the covenant UTXO. Split between continuation and permission outputs
    /// when [`Self::permission_spk_hash`] is non-zero (see
    /// [`Settlement::build_dev`](super::Settlement::build_dev)).
    pub value: u64,
    /// `blake2b(perm_redeem_script)` from the batch. Non-zero →
    /// [`Settlement::build_dev`](super::Settlement::build_dev) emits a second covenant-bound P2SH
    /// exit output of value [`Self::permission_output_value`] (count==2 layout). `[0; 32]` →
    /// single continuation output (count==1, no exits). Mirrors
    /// [`SettlementInput::permission_spk_hash`](super::SettlementInput::permission_spk_hash).
    pub permission_spk_hash: &'a [u8; 32],
    /// Sompi value split off into the permission-exit output (output index 1) and pinned into the
    /// dev redeem script for the count==2 continuation check. Ignored when `permission_spk_hash`
    /// is `[0; 32]`.
    pub permission_output_value: u64,
}
