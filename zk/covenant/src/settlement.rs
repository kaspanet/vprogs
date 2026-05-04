//! Host-side settlement transaction builder.
//!
//! Builds a Kaspa transaction that spends a covenant UTXO carrying
//! `(prev_state, prev_lane_tip)` and creates a single continuation output pinned to
//! `(new_state, new_lane_tip)`. The input's signature script provides the ZK-proof witness,
//! the chain block hash whose sequencing commitment anchors `new_seq_commit`, and the advanced
//! `(new_state, new_lane_tip)` pair the covenant script reconstructs into the next redeem
//! prefix.
//!
//! The ZK receipt supplied here must have committed the 224-byte settlement journal defined
//! by `StateTransition` in [`vprogs_zk_abi::batch_processor`]; this builder does not recompute
//! or verify it.

use kaspa_consensus_core::{
    constants::TX_VERSION_POST_COV_HF,
    subnets::SUBNETWORK_ID_NATIVE,
    tx::{CovenantBinding, Transaction, TransactionInput, TransactionOutpoint, TransactionOutput},
};
use kaspa_hashes::Hash;
use kaspa_txscript::{
    script_builder::ScriptBuilder, standard::pay_to_script_hash_script, zk_precompiles::tags::ZkTag,
};

use crate::script::{build_redeem_script, redeem_script_len};

/// Inputs describing a single settlement step.
pub struct SettlementInput<'a> {
    /// Covenant id carried forward by the continuation output.
    pub covenant_id: Hash,
    /// Batch processor guest image id the covenant pins against (the proof verifier).
    pub program_id: &'a [u8; 32],
    /// Transaction processor guest image id the covenant binds in the journal preimage so
    /// the inner-proof verifier identity is constrained on-chain. Hardcoded into the redeem
    /// script body.
    pub tx_image_id: &'a [u8; 32],
    /// L2 SMT state root before this batch.
    pub prev_state: &'a [u8; 32],
    /// Lane tip embedded in the covenant UTXO's redeem prefix (carried from the previous
    /// settlement).
    pub prev_lane_tip: &'a Hash,
    /// L2 SMT state root after this batch.
    pub new_state: &'a [u8; 32],
    /// Lane tip after this batch (locks into the continuation UTXO's redeem prefix and feeds
    /// into the guest's `seq_commit` derivation - rewind-resistant).
    pub new_lane_tip: &'a Hash,
    /// L1 chain block whose seq commitment the covenant script anchors `new_seq_commit` to.
    pub block_prove_to: Hash,
    /// UTXO outpoint of the covenant being spent.
    pub prev_outpoint: TransactionOutpoint,
    /// Value carried on the covenant UTXO (forwarded verbatim to the continuation output).
    pub value: u64,
    /// Risc0 succinct receipt witness bytes (see [`SuccinctWitness`]).
    pub witness: SuccinctWitness<'a>,
}

/// Serialized pieces of a risc0 succinct receipt that the covenant script consumes as the ZK
/// witness. These correspond to `SuccinctReceipt` fields the host pushes onto the script stack.
pub struct SuccinctWitness<'a> {
    /// STARK seal serialized as little-endian bytes of each `u32` word.
    pub seal: &'a [u8],
    /// 32-byte receipt claim digest.
    pub claim: &'a [u8; 32],
    /// 32-byte risc0 control id (binds the proof to a specific control root; kaspa PR #957).
    pub control_id: &'a [u8; 32],
    /// Hash function id (0 = blake2b, 1 = poseidon2, 2 = sha256).
    pub hashfn: u8,
    /// Control-inclusion-proof leaf index (little-endian u32).
    pub control_index: u32,
    /// Concatenated 32-byte control-inclusion-proof path digests.
    pub control_digests: &'a [u8],
}

/// A built settlement transaction and the redeem script it spends.
pub struct Settlement {
    /// The settlement transaction, ready to submit after mass/fee finalization.
    pub transaction: Transaction,
    /// Redeem script spent by input 0 (useful for debugging / SPK reconstruction).
    pub prev_redeem: Vec<u8>,
    /// Redeem script embedded in the continuation output (useful for the follow-on settlement).
    pub next_redeem: Vec<u8>,
}

impl Settlement {
    /// Builds the settlement transaction for a single batch.
    pub fn build(input: &SettlementInput<'_>) -> Self {
        let redeem_len = redeem_script_len(
            input.prev_state,
            input.program_id,
            input.tx_image_id,
            ZkTag::R0Succinct,
        );

        let prev_redeem = build_redeem_script(
            input.prev_state,
            input.prev_lane_tip,
            redeem_len,
            input.program_id,
            input.tx_image_id,
            ZkTag::R0Succinct,
        );
        let next_redeem = build_redeem_script(
            input.new_state,
            input.new_lane_tip,
            redeem_len,
            input.program_id,
            input.tx_image_id,
            ZkTag::R0Succinct,
        );

        let sig_script = sig_script(
            &prev_redeem,
            input.block_prove_to,
            input.new_state,
            input.new_lane_tip,
            &input.witness,
        );

        let tx_input = TransactionInput::new(input.prev_outpoint, sig_script, 0, 1);
        let tx_output = TransactionOutput::with_covenant(
            input.value,
            pay_to_script_hash_script(&next_redeem),
            Some(CovenantBinding::new(0, input.covenant_id)),
        );

        let tx = Transaction::new(
            TX_VERSION_POST_COV_HF,
            vec![tx_input],
            vec![tx_output],
            0,
            SUBNETWORK_ID_NATIVE,
            0,
            Vec::new(),
        );

        Self { transaction: tx, prev_redeem, next_redeem }
    }
}

/// Builds the signature script witness layout consumed by the covenant's redeem script.
///
/// Push order (bottom to top):
/// `[claim, control_index, control_digests, seal, control_id, hashfn,
///   block_prove_to, new_state, new_lane_tip, redeem]`.
///
/// After the P2SH check pops `redeem`, the redeem execution stashes `prev_lane_tip` /
/// `prev_state` from its own prefix, consumes `block_prove_to` via `OpChainblockSeqCommit`,
/// stashes `new_state` / `new_lane_tip` / `new_seq_commit` for the journal, and finishes
/// with `Op2Swap + OpZkPrecompile` consuming the remaining 8 items in the R0Succinct layout.
fn sig_script(
    redeem: &[u8],
    block_prove_to: Hash,
    new_state: &[u8; 32],
    new_lane_tip: &Hash,
    witness: &SuccinctWitness<'_>,
) -> Vec<u8> {
    ScriptBuilder::new()
        .add_data(witness.claim)
        .unwrap()
        .add_data(&witness.control_index.to_le_bytes())
        .unwrap()
        .add_data(witness.control_digests)
        .unwrap()
        .add_data(witness.seal)
        .unwrap()
        .add_data(witness.control_id)
        .unwrap()
        .add_data(&[witness.hashfn])
        .unwrap()
        .add_data(block_prove_to.as_slice())
        .unwrap()
        .add_data(new_state)
        .unwrap()
        .add_data(new_lane_tip.as_slice())
        .unwrap()
        .add_data(redeem)
        .unwrap()
        .drain()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settlement_tx_has_single_covenant_output() {
        let input = SettlementInput {
            covenant_id: Hash::from_bytes([0xAA; 32]),
            program_id: &[0xBB; 32],
            tx_image_id: &[0xCC; 32],
            prev_state: &[0x11; 32],
            prev_lane_tip: &Hash::from_bytes([0x22; 32]),
            new_state: &[0x33; 32],
            new_lane_tip: &Hash::from_bytes([0x44; 32]),
            block_prove_to: Hash::from_bytes([0x55; 32]),
            prev_outpoint: TransactionOutpoint::new(Hash::from_bytes([0x66; 32]), 0),
            value: 100_000_000,
            witness: SuccinctWitness {
                seal: &[0u8; 8],
                claim: &[0u8; 32],
                control_id: &[0u8; 32],
                hashfn: 0,
                control_index: 0,
                control_digests: &[0u8; 0],
            },
        };

        let settlement = Settlement::build(&input);

        assert_eq!(settlement.transaction.inputs.len(), 1);
        assert_eq!(settlement.transaction.outputs.len(), 1);
        let output = &settlement.transaction.outputs[0];
        assert_eq!(output.value, 100_000_000);
        assert_eq!(
            output.covenant,
            Some(CovenantBinding::new(0, Hash::from_bytes([0xAA; 32]))),
            "continuation output must preserve covenant id",
        );
        assert_ne!(
            settlement.prev_redeem, settlement.next_redeem,
            "next redeem must embed advanced state",
        );
    }
}
