//! Builds a signed carrier transaction whose L2 payload signs over the final `rest_preimage`,
//! funded from a single UTXO and priced under the fee policy.

use kaspa_addresses::Address;
use kaspa_consensus_core::{
    config::params::Params,
    hashing::tx::transaction_v1_rest_preimage,
    mass::UtxoCell,
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
    pricing::{FeePolicy, min_fee, priority_mass, required_fee},
    viability::{commit_storage_mass, min_viable_change},
};

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
    /// How the fee is priced.
    pub fee_policy: FeePolicy,
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
    let floor = min_fee(args.params, &probe);
    // Floor pricing is the legacy behavior unchanged; a target rate runs the same fixpoint as
    // the funding walk, but over the single input a carrier can spend, with no prefix walk and
    // no error path: the carrier cannot add inputs, so an unreachable target degrades to the
    // affordable cap (never below the floor) rather than failing the build.
    let fee = match args.fee_policy {
        FeePolicy::Floor => floor,
        FeePolicy::TargetFeerate(_) => {
            let available = args.entry.amount - extra_value;
            let input_cells: Vec<UtxoCell> = entries.iter().map(UtxoCell::from).collect();
            let output_cells: Vec<UtxoCell> = probe.outputs.iter().map(UtxoCell::from).collect();
            match min_viable_change(args.params, &input_cells, &output_cells) {
                // Storage-infeasible change keeps the floor fee; the assert below still guards.
                None => floor,
                Some(viable) => {
                    let cap = available.saturating_sub(viable);
                    let params = args.params;
                    let pm = |change: u64| {
                        priority_mass(params, &probe, &input_cells, &output_cells, change)
                    };
                    match required_fee(args.fee_policy, floor, available, cap, pm) {
                        Ok(fee) => fee,
                        // Below the floor the cap can never be a fee: keep the floor.
                        Err(_) if cap >= floor => cap,
                        Err(_) => floor,
                    }
                }
            }
        }
    };
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

/// Regression tests for [`signed_carrier_transaction`]'s fee pricing under
/// [`FeePolicy::Floor`] and [`FeePolicy::TargetFeerate`].
#[cfg(test)]
mod tests {
    use kaspa_consensus_core::{
        config::params::SIMNET_PARAMS, constants::TX_VERSION_TOCCATA, subnets::SUBNETWORK_ID_NATIVE,
    };
    use kaspa_txscript::pay_to_address_script;

    use super::*;
    use crate::build::{
        funding::fee_paid,
        testing::{address, keypair, outpoint, priority_mass_mirror, target_fee_mirror},
    };

    /// A constant-length payload stand-in (the L2 signature is fixed-size), as a named fn so
    /// it satisfies `Fn(&[u8])` for every lifetime.
    fn payload(_: &[u8]) -> Vec<u8> {
        vec![0u8; 64]
    }

    /// A carrier built with `funding` sompi of input, no extra outputs, and a constant-length
    /// payload, priced under `fee_policy`.
    fn carrier(params: &Params, fee_policy: FeePolicy, funding: u64) -> (Transaction, UtxoEntry) {
        let keypair = keypair();
        let address = address(&keypair, params);
        let entry = UtxoEntry::new(funding, pay_to_address_script(&address), 0, false, None);
        let tx = signed_carrier_transaction(SignedCarrierTx {
            outpoint: outpoint(1),
            entry: entry.clone(),
            keypair,
            change_address: &address,
            subnetwork_id: SUBNETWORK_ID_NATIVE,
            tx_version: TX_VERSION_TOCCATA,
            params,
            fee_policy,
            extra_outputs: vec![],
            finalize_payload: payload,
        });
        (tx, entry)
    }

    /// A target rate the funding UTXO comfortably reaches: the carrier pays it over its final
    /// priority mass, strictly above the mempool floor.
    #[test]
    fn carrier_pays_the_target_feerate() {
        let params = &SIMNET_PARAMS;
        let rate = 500.0;
        let (tx, entry) = carrier(params, FeePolicy::TargetFeerate(rate), 1_000_000_000);

        let paid = fee_paid(&tx, std::slice::from_ref(&entry));
        assert_eq!(paid, target_fee_mirror(params, rate, &tx, std::slice::from_ref(&entry)));
        assert!(paid > min_fee(params, &tx), "target fee {paid} must out-bid the floor");
        assert!(paid as f64 / priority_mass_mirror(params, &tx, &[entry]) as f64 >= rate);
    }

    /// A target rate no single-UTXO carrier can reach degrades to the affordable cap: the
    /// change lands on its storage-viable minimum and the rest of the available value pays
    /// the fee.
    #[test]
    fn carrier_degrades_to_the_cap_when_the_target_is_unreachable() {
        let params = &SIMNET_PARAMS;
        let funding = 1_000_000_000u64;
        let (tx, entry) = carrier(params, FeePolicy::TargetFeerate(1_000_000.0), funding);

        let input_cells: Vec<UtxoCell> = std::iter::once(&entry).map(UtxoCell::from).collect();
        let output_cells: Vec<UtxoCell> = tx.outputs.iter().map(UtxoCell::from).collect();
        let viable =
            min_viable_change(params, &input_cells, &output_cells).expect("shape is feasible");
        assert_eq!(tx.outputs.len(), 1, "the change output is the carrier's only output");
        assert_eq!(tx.outputs[0].value, viable);
        assert_eq!(fee_paid(&tx, &[entry]), funding - viable);
    }

    /// Floor pricing keeps the legacy behavior: the carrier pays exactly `min_fee` of its own
    /// layout.
    #[test]
    fn carrier_floor_policy_pays_the_min_fee() {
        let params = &SIMNET_PARAMS;
        let (tx, entry) = carrier(params, FeePolicy::Floor, 1_000_000_000);

        assert_eq!(fee_paid(&tx, &[entry]), min_fee(params, &tx));
    }
}
