//! Host-side bootstrap transaction builder.
//!
//! Creates the initial covenant UTXO from a regular funding UTXO. Because the spending input does
//! not yet carry a covenant id, the output is validated via genesis-covenant-id reconstruction:
//! the consensus validator recomputes `covenant_id(input.outpoint, [(0, output)])` and compares it
//! against the binding. This builder returns the computed id so the caller can use it in the
//! first settlement step.

use kaspa_consensus_core::{
    constants::TX_VERSION_POST_COV_HF,
    hashing::covenant_id::covenant_id,
    subnets::SUBNETWORK_ID_NATIVE,
    tx::{CovenantBinding, Transaction, TransactionInput, TransactionOutpoint, TransactionOutput},
};
use kaspa_hashes::Hash;
use kaspa_txscript::{standard::pay_to_script_hash_script, zk_precompiles::tags::ZkTag};

use crate::script::{build_redeem_script, redeem_script_len};

/// Inputs describing an initial covenant UTXO bootstrap.
pub struct BootstrapInput<'a> {
    /// Batch processor guest image id the covenant pins against (the proof verifier).
    pub program_id: &'a [u8; 32],
    /// Transaction processor guest image id hardcoded into the redeem script (binds the
    /// inner-proof verifier identity into the journal preimage).
    pub tx_image_id: &'a [u8; 32],
    /// Initial L2 SMT state root (typically `EMPTY_HASH`).
    pub initial_state: &'a [u8; 32],
    /// Initial lane tip embedded in the genesis covenant UTXO's redeem prefix (typically zero).
    pub initial_lane_tip: &'a Hash,
    /// Funding outpoint supplying the covenant UTXO's value.
    pub funding_outpoint: TransactionOutpoint,
    /// Amount to lock into the covenant UTXO (must be funded by the input).
    pub value: u64,
}

/// A built bootstrap transaction and the derived covenant id it creates.
pub struct Bootstrap {
    /// Unsigned transaction ready to fund-sign and submit.
    pub transaction: Transaction,
    /// Covenant id attached to output 0; feeds [`crate::SettlementInput::covenant_id`].
    pub covenant_id: Hash,
    /// Redeem script embedded in output 0; useful for constructing follow-on settlements.
    pub initial_redeem: Vec<u8>,
}

impl Bootstrap {
    /// Builds the bootstrap transaction.
    pub fn build(input: &BootstrapInput<'_>) -> Self {
        let redeem_len = redeem_script_len(
            input.initial_state,
            input.program_id,
            input.tx_image_id,
            ZkTag::R0Succinct,
        );
        let initial_redeem = build_redeem_script(
            input.initial_state,
            input.initial_lane_tip,
            redeem_len,
            input.program_id,
            input.tx_image_id,
            ZkTag::R0Succinct,
        );

        // The caller signs the input after building; the signature fills the sig_script in place.
        let tx_input = TransactionInput::new(input.funding_outpoint, Vec::new(), 0, 1);

        // Compute the covenant id from the genesis outpoint + [(0, output-without-binding)]: the
        // consensus validator uses the same recipe before binding is attached.
        let provisional_output =
            TransactionOutput::new(input.value, pay_to_script_hash_script(&initial_redeem));
        let covenant_id =
            covenant_id(input.funding_outpoint, core::iter::once((0u32, &provisional_output)));

        let tx_output = TransactionOutput::with_covenant(
            input.value,
            pay_to_script_hash_script(&initial_redeem),
            Some(CovenantBinding::new(0, covenant_id)),
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

        Self { transaction: tx, covenant_id, initial_redeem }
    }
}
