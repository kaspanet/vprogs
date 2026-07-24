//! Funds and signs a covenant-spending settlement transaction without submitting it, appending
//! fee inputs from a candidate prefix while preserving the covenant input's witness.

use kaspa_addresses::Address;
use kaspa_consensus_core::{
    config::params::Params,
    mass::units::ComputeBudget,
    sign::sign,
    tx::{
        MutableTransaction, Transaction, TransactionInput, TransactionOutpoint, TransactionOutput,
        UtxoEntry,
    },
};
use kaspa_txscript::pay_to_address_script;
use secp256k1::Keypair;

use super::{
    funding::{BuildError, fund},
    pricing::FeePolicy,
    viability::commit_storage_mass,
};

/// Inputs to [`settlement_transaction`].
pub struct SettlementTx<'a> {
    /// The covenant-spending settlement tx (its input 0 already carries the covenant witness).
    pub settlement_tx: Transaction,
    /// Entry of the covenant UTXO being spent by input 0.
    pub covenant_entry: UtxoEntry,
    /// Compute budget to set on the covenant input.
    pub covenant_compute_budget: ComputeBudget,
    /// Fee-funding UTXOs in preference order; the builder spends a prefix of this list.
    pub fee_candidates: Vec<(TransactionOutpoint, UtxoEntry)>,
    /// Key that signs the fee inputs.
    pub keypair: Keypair,
    /// Address the change is paid back to.
    pub address: &'a Address,
    /// How the fee is priced.
    pub fee_policy: FeePolicy,
    /// Consensus params, for the mass-based fee and storage mass.
    pub params: &'a Params,
}

/// Funds and signs a settlement transaction without submitting it: appends fee inputs from a
/// prefix of `args.fee_candidates` plus a change output when the remainder is storage-viable
/// (folding it into the fee otherwise), signs the fee inputs, preserves the covenant input's
/// witness, sets the covenant input's compute budget, and commits the storage-mass field.
/// The result is ready to submit.
pub fn settlement_transaction(args: SettlementTx<'_>) -> Result<Transaction, BuildError> {
    // Snapshot the covenant witness before signing - `sign` overwrites all signature scripts.
    let covenant_sig_script = args.settlement_tx.inputs[0].signature_script.clone();
    let change_spk = pay_to_address_script(args.address);
    let mut build = |n: usize, change: Option<u64>| {
        let mut tx = args.settlement_tx.clone();
        tx.inputs.extend(
            args.fee_candidates[..n]
                .iter()
                .map(|(outpoint, _)| TransactionInput::new(*outpoint, vec![], 0, 1)),
        );
        tx.outputs.extend(
            change.map(|value| TransactionOutput::with_covenant(value, change_spk.clone(), None)),
        );
        let entries: Vec<UtxoEntry> = std::iter::once(args.covenant_entry.clone())
            .chain(args.fee_candidates[..n].iter().map(|(_, entry)| entry.clone()))
            .collect();
        let mut tx = sign(MutableTransaction::with_entries(tx, entries.clone()), args.keypair).tx;
        tx.inputs[0].signature_script = covenant_sig_script.clone();
        tx.inputs[0].compute_commit = args.covenant_compute_budget.into();
        (tx, entries)
    };
    let funding = fund(args.params, args.fee_policy, &args.fee_candidates, false, &mut build)?;
    let (mut tx, entries) = build(funding.inputs, funding.change);
    commit_storage_mass(args.params, &tx, &entries);
    // The signature script on input 0 changed, so recompute the on-the-wire id.
    tx.finalize();
    Ok(tx)
}

/// Regression tests for [`settlement_transaction`]'s fee pricing on a storage-dominated
/// covenant/permission layout and on a transient-mass-dominated large covenant witness.
#[cfg(test)]
mod tests {
    use kaspa_consensus_core::config::params::SIMNET_PARAMS;
    use kaspa_hashes::Hash;
    use kaspa_txscript::standard::pay_to_script_hash_script;

    use super::*;
    use crate::build::{
        funding::fee_paid,
        testing::{
            DEV_COVENANT_BUDGET, FUND_VALUE, PERMISSION_OUTPUT_VALUE, address,
            assert_fee_covers_final, keypair, mempool_min_fee, outpoint, settlement_shape,
            settlement_skeleton,
        },
    };

    /// A storage-dominated dev-mode settlement, built through [`settlement_transaction`]: its
    /// covenant and permission outputs put storage mass far above compute mass, pinning that
    /// `min_fee` still covers the final transaction and stays block-fit even when storage mass
    /// is the larger term.
    #[test]
    fn settlement_fee_covers_the_built_transactions_floor() {
        let params = &SIMNET_PARAMS;
        let shape = settlement_shape(params);
        assert_fee_covers_final(params, &shape.tx, &shape.entries);
    }

    /// A settlement with a large covenant witness is transient-mass-dominated (2 grams per
    /// byte against compute's ~1): the fee must be priced on normalized transient mass.
    #[test]
    fn witness_heavy_settlement_fee_covers_the_transient_floor() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let covenant_id = Hash::from_bytes([9u8; 32]);
        let covenant_spk = pay_to_script_hash_script(&[0xABu8; 64]);
        let covenant_value = 100_000_000;
        let covenant_entry =
            UtxoEntry::new(covenant_value, covenant_spk.clone(), 0, false, Some(covenant_id));
        let fee_entry = UtxoEntry::new(FUND_VALUE, pay_to_address_script(&address), 0, false, None);

        let tx = settlement_transaction(SettlementTx {
            settlement_tx: settlement_skeleton(
                &covenant_spk,
                covenant_id,
                covenant_value - PERMISSION_OUTPUT_VALUE,
                &address,
                100_000,
            ),
            covenant_entry: covenant_entry.clone(),
            covenant_compute_budget: DEV_COVENANT_BUDGET,
            fee_candidates: vec![(outpoint(3), fee_entry.clone())],
            keypair,
            address: &address,
            fee_policy: FeePolicy::Floor,
            params,
        })
        .expect("settlement is fundable");

        let entries = vec![covenant_entry, fee_entry];
        let paid = fee_paid(&tx, &entries);
        let floor = mempool_min_fee(params, &tx);
        assert!(paid >= floor, "built tx pays {paid} but the node's floor is {floor}");
    }
}
