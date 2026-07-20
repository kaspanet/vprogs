//! Builds a signed activity transaction that carries an arbitrary payload on a subnetwork,
//! funded from a candidate prefix and paying its remainder back to a single address.

use kaspa_addresses::Address;
use kaspa_consensus_core::{
    config::params::Params,
    sign::sign,
    subnets::SubnetworkId,
    tx::{
        MutableTransaction, Transaction, TransactionInput, TransactionOutpoint, TransactionOutput,
        UtxoEntry,
    },
};
use kaspa_txscript::pay_to_address_script;
use secp256k1::Keypair;

use super::{
    funding::{BuildError, fee_paid, fund},
    pricing::FeePolicy,
    viability::commit_storage_mass,
};

/// Inputs to [`activity_transaction`].
pub struct ActivityTx<'a> {
    /// Payload carried by the transaction (e.g. an encoded activity instruction).
    pub payload: Vec<u8>,
    /// Funding UTXOs in preference order; the builder spends a prefix of this list.
    pub candidates: Vec<(TransactionOutpoint, UtxoEntry)>,
    /// Key that signs the inputs.
    pub keypair: Keypair,
    /// Address the remainder (after fee) is paid back to.
    pub address: &'a Address,
    /// Subnetwork the tx is issued on; a lane subnetwork for activity, else native.
    pub subnetwork_id: SubnetworkId,
    /// Transaction version; [`TX_VERSION_TOCCATA`] or newer for non-native subnetworks.
    pub tx_version: u16,
    /// How the fee is priced.
    pub fee_policy: FeePolicy,
    /// Consensus params, for the mass-based fee.
    pub params: &'a Params,
}

/// Builds one signed activity transaction funded from a prefix of `args.candidates`, paying
/// the remainder (after the node's minimum mass-based fee) back to `args.address`, carrying
/// `args.payload` on `args.subnetwork_id`.
pub fn activity_transaction(args: ActivityTx<'_>) -> Result<Transaction, BuildError> {
    let spk = pay_to_address_script(args.address);
    let mut build = |n: usize, change: Option<u64>| {
        let inputs = args.candidates[..n]
            .iter()
            .map(|(outpoint, _)| TransactionInput::new(*outpoint, vec![], 0, 1))
            .collect();
        let out_amount = change.expect("the activity output is required");
        let tx = Transaction::new(
            args.tx_version,
            inputs,
            vec![TransactionOutput::new(out_amount, spk.clone())],
            0,
            args.subnetwork_id,
            0,
            args.payload.clone(),
        );
        let entries: Vec<UtxoEntry> =
            args.candidates[..n].iter().map(|(_, entry)| entry.clone()).collect();
        (sign(MutableTransaction::with_entries(tx, entries.clone()), args.keypair).tx, entries)
    };
    let funding = fund(args.params, args.fee_policy, &args.candidates, true, &mut build)?;
    let (tx, entries) = build(funding.inputs, funding.change);
    debug_assert_eq!(fee_paid(&tx, &entries), funding.fee, "tx must pay what `fund` computed");
    commit_storage_mass(args.params, &tx, &entries);
    Ok(tx)
}

/// Regression tests for [`activity_transaction`]'s fee pricing on a `C/v`-dominated single
/// output and on a transient-mass-dominated large payload.
#[cfg(test)]
mod tests {
    use kaspa_consensus_core::{
        config::params::SIMNET_PARAMS, constants::TX_VERSION_TOCCATA, mass::MassCalculator,
        subnets::SUBNETWORK_ID_NATIVE, tx::PopulatedTransaction,
    };

    use super::*;
    use crate::build::testing::{
        activity_shape, address, assert_fee_covers_final, keypair, mempool_min_fee, outpoint,
    };

    /// A small-UTXO activity transaction, built through [`activity_transaction`], where the
    /// single output's `C/v` term dominates compute mass, pinning that `min_fee` still covers
    /// the final transaction and stays block-fit for that shape. Also pins that the tx's
    /// committed `storage_mass()` field matches the value
    /// [`MassCalculator::calc_contextual_masses`] recomputes on the populated tx, confirming
    /// [`commit_storage_mass`] took effect.
    #[test]
    fn activity_fee_covers_the_built_transactions_floor() {
        let params = &SIMNET_PARAMS;
        let shape = activity_shape(params);
        assert_fee_covers_final(params, &shape.tx, &shape.entries);

        let calc = MassCalculator::new(
            params.mass_per_tx_byte,
            params.mass_per_script_pub_key_byte,
            params.storage_mass_parameter,
        );
        let populated = PopulatedTransaction::new(&shape.tx, shape.entries.clone());
        let recomputed = calc
            .calc_contextual_masses(&populated)
            .expect("contextual mass calculation must succeed for a populated transaction")
            .storage_mass;
        assert_eq!(
            shape.tx.storage_mass(),
            recomputed,
            "committed storage mass must match the value recomputed on the populated tx",
        );
    }

    /// A payload-heavy activity tx is transient-mass-dominated: normalized transient mass is
    /// 2 grams per serialized byte against compute's ~1, so a large payload puts the node's
    /// fee floor above what compute-only pricing pays.
    #[test]
    fn payload_heavy_activity_fee_covers_the_transient_floor() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let entry = UtxoEntry::new(100_000_000, pay_to_address_script(&address), 0, false, None);

        let tx = activity_transaction(ActivityTx {
            payload: vec![0u8; 10_000],
            candidates: vec![(outpoint(1), entry.clone())],
            keypair,
            address: &address,
            subnetwork_id: SUBNETWORK_ID_NATIVE,
            tx_version: TX_VERSION_TOCCATA,
            fee_policy: FeePolicy::Floor,
            params,
        })
        .expect("activity is fundable");

        let paid = fee_paid(&tx, &[entry]);
        let floor = mempool_min_fee(params, &tx);
        assert!(paid >= floor, "built tx pays {paid} but the node's floor is {floor}");
    }
}
