//! Builds a signed transaction paying equal-value outputs to one recipient, funded from a
//! candidate prefix and returning any storage-viable remainder as change.

use kaspa_addresses::Address;
use kaspa_consensus_core::{
    config::params::Params,
    constants::TX_VERSION_TOCCATA,
    sign::sign,
    subnets::SUBNETWORK_ID_NATIVE,
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

/// Inputs to [`pay_to_address_transaction`].
pub struct PayToAddressTx<'a> {
    /// Funding UTXOs in preference order; the builder spends a prefix of this list.
    pub candidates: Vec<(TransactionOutpoint, UtxoEntry)>,
    /// Recipient address each of the `count` outputs pays.
    pub recipient: &'a Address,
    /// Sompi paid to each recipient output.
    pub value: u64,
    /// Number of equal-value outputs to the recipient.
    pub count: usize,
    /// Key that funds the inputs and receives the change.
    pub keypair: Keypair,
    /// Address the remainder (after the recipient outputs and the fee) is paid back to.
    pub change_address: &'a Address,
    /// Consensus params, for the mass-based fee and storage mass.
    pub params: &'a Params,
}

/// Builds one signed transaction paying `args.count` outputs of `args.value` sompi each to
/// `args.recipient`, funded from a prefix of `args.candidates`, returning the remainder
/// (after those outputs and the node's minimum mass-based fee) as change to
/// `args.change_address` when the remainder is storage-viable, and folding it into the fee
/// otherwise. Used to seed a distinct prover's funding address from a coinbase wallet.
pub fn pay_to_address_transaction(args: PayToAddressTx<'_>) -> Result<Transaction, BuildError> {
    let recipient_spk = pay_to_address_script(args.recipient);
    let change_spk = pay_to_address_script(args.change_address);
    args.value.checked_mul(args.count as u64).expect("recipient payout overflow");
    let mut build = |n: usize, change: Option<u64>| {
        let inputs = args.candidates[..n]
            .iter()
            .map(|(outpoint, _)| TransactionInput::new(*outpoint, vec![], 0, 1))
            .collect();
        let mut outputs: Vec<TransactionOutput> = (0..args.count)
            .map(|_| TransactionOutput::new(args.value, recipient_spk.clone()))
            .collect();
        outputs.extend(change.map(|value| TransactionOutput::new(value, change_spk.clone())));
        let tx = Transaction::new(
            TX_VERSION_TOCCATA,
            inputs,
            outputs,
            0,
            SUBNETWORK_ID_NATIVE,
            0,
            Vec::new(),
        );
        let entries: Vec<UtxoEntry> =
            args.candidates[..n].iter().map(|(_, entry)| entry.clone()).collect();
        (sign(MutableTransaction::with_entries(tx, entries.clone()), args.keypair).tx, entries)
    };
    let funding = fund(args.params, FeePolicy::Floor, &args.candidates, false, &mut build)?;
    let (tx, entries) = build(funding.inputs, funding.change);
    commit_storage_mass(args.params, &tx, &entries);
    Ok(tx)
}

/// Regression tests for [`pay_to_address_transaction`]'s fee pricing and its change-drop
/// behavior when the funding slack cannot cover a storage-viable change.
#[cfg(test)]
mod tests {
    use kaspa_consensus_core::config::params::SIMNET_PARAMS;

    use super::*;
    use crate::build::{
        funding::fee_paid,
        testing::{address, assert_fee_covers_final, keypair, outpoint, pay_to_address_shape},
    };

    /// A small-change payout, built through [`pay_to_address_transaction`], where the change's
    /// `C/v` term dominates compute mass, pinning that `min_fee` still covers the final
    /// transaction and stays block-fit for that shape.
    #[test]
    fn pay_to_address_fee_covers_the_built_transactions_floor() {
        let params = &SIMNET_PARAMS;
        let shape = pay_to_address_shape(params);
        assert_fee_covers_final(params, &shape.tx, &shape.entries);
    }

    /// A funding UTXO whose slack above the payout covers the fee but not a storage-viable
    /// change: the change output is dropped and the slack folds into the fee.
    #[test]
    fn pay_to_address_drops_an_unviable_change() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let payout = 50_000_000;
        // 400_000 sompi of slack: above the fee, far below the ~2M viable-change floor.
        let entry =
            UtxoEntry::new(payout + 400_000, pay_to_address_script(&address), 0, false, None);

        let tx = pay_to_address_transaction(PayToAddressTx {
            candidates: vec![(outpoint(1), entry.clone())],
            recipient: &address,
            value: payout,
            count: 1,
            keypair,
            change_address: &address,
            params,
        })
        .expect("fundable without a change output");

        assert_eq!(tx.outputs.len(), 1, "change output must be dropped");
        assert_eq!(fee_paid(&tx, &[entry]), 400_000);
    }
}
