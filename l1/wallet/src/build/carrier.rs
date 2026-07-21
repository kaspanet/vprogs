//! Builds a signed carrier transaction whose L2 payload signs over the final `rest_preimage`,
//! funded from a single UTXO at the mempool floor.

use kaspa_addresses::Address;
use kaspa_consensus_core::{
    config::params::Params,
    hashing::tx::transaction_v1_rest_preimage,
    sign::sign,
    subnets::SubnetworkId,
    tx::{
        MutableTransaction, Transaction, TransactionInput, TransactionOutpoint, TransactionOutput,
        UtxoEntry,
    },
};
use kaspa_txscript::pay_to_address_script;
use secp256k1::Keypair;

use super::{pricing::min_fee, viability::commit_storage_mass};

/// Inputs to [`signed_carrier_transaction`].
pub struct SignedCarrierTx<'a, F> {
    /// The funding outpoint to spend (its key signs the input).
    pub outpoint: TransactionOutpoint,
    /// The funding outpoint's entry.
    pub entry: UtxoEntry,
    /// Key that signs (and funds) the input.
    pub keypair: Keypair,
    /// Address the change (after extra outputs and the fee) is paid back to.
    pub change_address: &'a Address,
    /// Subnetwork the carrier rides (a lane subnetwork for L2 activity).
    pub subnetwork_id: SubnetworkId,
    /// Transaction version;
    /// [`TX_VERSION_TOCCATA`](kaspa_consensus_core::constants::TX_VERSION_TOCCATA) or newer for
    /// non-native subnetworks.
    pub tx_version: u16,
    /// Consensus params, for the mass-based fee and storage mass.
    pub params: &'a Params,
    /// Extra outputs prepended before the change output (e.g. a deposit funding output). Their
    /// indices in the final tx are `0..extra_outputs.len()`.
    pub extra_outputs: Vec<TransactionOutput>,
    /// Produces the full L2 payload given the carrier's `rest_preimage`. Called once with an empty
    /// rest to size the payload (for the fee probe) and once with the real rest to sign over the
    /// final outputs. Must return a constant-length payload (the L2 signature is fixed-size).
    pub finalize_payload: F,
}

/// Builds a signed carrier transaction whose L2 payload is produced by `finalize_payload` over the
/// transaction's `rest_preimage`. Used for runtime carriers whose payload signature commits to the
/// (post-funding) outputs: the fee is priced from a same-size probe, the change is fixed, then the
/// payload is signed over the final `rest_preimage` and the input is L1-signed.
///
/// `rest_preimage` excludes the payload, signature scripts, and mass, so it is stable across the
/// payload splice and the L1 signing; computing it from the final-output skeleton matches the
/// submitted transaction.
pub fn signed_carrier_transaction<F: Fn(&[u8]) -> Vec<u8>>(
    args: SignedCarrierTx<'_, F>,
) -> Transaction {
    let input = TransactionInput::new(args.outpoint, vec![], 0, 1);
    let change_spk = pay_to_address_script(args.change_address);
    let entries = vec![args.entry.clone()];
    let extra_value: u64 = args.extra_outputs.iter().map(|o| o.value).sum();
    assert!(
        args.entry.amount > extra_value,
        "funding UTXO amount {} too small for extra outputs {}",
        args.entry.amount,
        extra_value,
    );

    let build = |fee: u64, payload: Vec<u8>| {
        let mut outputs = args.extra_outputs.clone();
        outputs.push(TransactionOutput::new(
            args.entry.amount - extra_value - fee,
            change_spk.clone(),
        ));
        let tx = Transaction::new(
            args.tx_version,
            vec![input.clone()],
            outputs,
            0,
            args.subnetwork_id,
            0,
            payload,
        );
        let signed = sign(MutableTransaction::with_entries(tx, entries.clone()), args.keypair).tx;
        commit_storage_mass(args.params, &signed, &entries);
        signed
    };

    // The payload length is independent of the rest_preimage, so an empty-rest probe sizes the tx.
    let probe_payload = (args.finalize_payload)(&[]);
    let probe = build(0, probe_payload.clone());
    let fee = min_fee(args.params, &probe);
    assert!(
        args.entry.amount > extra_value + fee,
        "funding UTXO amount {} too small for extra outputs {} + fee {}",
        args.entry.amount,
        extra_value,
        fee,
    );

    // Outputs (including the final change) are now fixed; compute the real rest_preimage from a
    // payload-less skeleton with those outputs and sign the payload over it.
    let mut skeleton_outputs = args.extra_outputs.clone();
    skeleton_outputs
        .push(TransactionOutput::new(args.entry.amount - extra_value - fee, change_spk.clone()));
    let skeleton = Transaction::new(
        args.tx_version,
        vec![input.clone()],
        skeleton_outputs,
        0,
        args.subnetwork_id,
        0,
        Vec::new(),
    );
    let rest = transaction_v1_rest_preimage(&skeleton);
    let final_payload = (args.finalize_payload)(&rest);
    assert_eq!(
        final_payload.len(),
        probe_payload.len(),
        "finalize_payload must return a constant-length payload",
    );

    build(fee, final_payload)
}
