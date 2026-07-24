//! Builds the genesis covenant-bootstrap transaction that seeds a P2SH covenant output from a
//! single funding UTXO.

use kaspa_consensus_core::{
    config::params::Params,
    constants::TX_VERSION_TOCCATA,
    hashing::covenant_id::covenant_id,
    sign::sign,
    subnets::SUBNETWORK_ID_NATIVE,
    tx::{
        CovenantBinding, MutableTransaction, Transaction, TransactionInput, TransactionOutpoint,
        TransactionOutput, UtxoEntry,
    },
};
use kaspa_hashes::Hash;
use kaspa_txscript::standard::pay_to_script_hash_script;
use secp256k1::Keypair;

use super::viability::commit_storage_mass;

/// Builds a signed bootstrap transaction whose single output is P2SH(`redeem_script`) with a
/// genesis covenant binding, funded by `(outpoint, entry)`. Returns the tx and the covenant id
/// consensus will recompute from the input outpoint and output.
pub fn covenant_bootstrap_transaction(
    redeem_script: &[u8],
    value: u64,
    outpoint: TransactionOutpoint,
    entry: UtxoEntry,
    keypair: Keypair,
    params: &Params,
) -> (Transaction, Hash) {
    let covenant_spk = pay_to_script_hash_script(redeem_script);
    let covenant_id = {
        let provisional = TransactionOutput::new(value, covenant_spk.clone());
        covenant_id(outpoint, std::iter::once((0u32, &provisional)))
    };

    let tx_input = TransactionInput::new(outpoint, Vec::new(), 0, 1);
    let tx_output = TransactionOutput::with_covenant(
        value,
        covenant_spk,
        Some(CovenantBinding::new(0, covenant_id)),
    );

    let unsigned = Transaction::new(
        TX_VERSION_TOCCATA,
        vec![tx_input],
        vec![tx_output],
        0,
        SUBNETWORK_ID_NATIVE,
        0,
        Vec::new(),
    );

    let entries = vec![entry];
    let signed = sign(MutableTransaction::with_entries(unsigned, entries.clone()), keypair).tx;
    commit_storage_mass(params, &signed, &entries);

    (signed, covenant_id)
}
