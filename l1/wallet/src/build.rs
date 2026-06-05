//! Pure L1 transaction construction over an already-chosen funding UTXO.
//!
//! Every function here is RPC-free: the caller supplies the spendable `(outpoint, entry)` it
//! picked, and gets back a signed, finalized transaction (with storage mass committed where
//! required). [`crate::Wallet`] wraps these with UTXO fetching and submission; an in-process
//! simulation calls them directly with UTXOs it already holds.

use kaspa_addresses::Address;
use kaspa_consensus_core::{
    config::params::Params,
    constants::TX_VERSION_TOCCATA,
    hashing::covenant_id::covenant_id,
    mass::{MassCalculator, units::ComputeBudget},
    sign::sign,
    subnets::{SUBNETWORK_ID_NATIVE, SubnetworkId},
    tx::{
        CovenantBinding, MutableTransaction, PopulatedTransaction, Transaction, TransactionInput,
        TransactionOutpoint, TransactionOutput, UtxoEntry,
    },
};
use kaspa_hashes::Hash;
use kaspa_txscript::{pay_to_address_script, standard::pay_to_script_hash_script};
use secp256k1::Keypair;

/// Commits KIP-0009 storage mass on `tx` (Toccata txs must carry it for the node to accept them).
pub fn commit_storage_mass(params: &Params, tx: &Transaction, entries: &[UtxoEntry]) {
    let calc = MassCalculator::new(
        params.mass_per_tx_byte,
        params.mass_per_script_pub_key_byte,
        params.storage_mass_parameter,
    );
    let populated = PopulatedTransaction::new(tx, entries.to_vec());
    let masses = calc
        .calc_contextual_masses(&populated)
        .expect("contextual mass calculation must succeed for a populated transaction");
    tx.set_mass(masses.storage_mass);
}

/// Inputs to [`activity_transaction`].
pub struct ActivityTx<'a> {
    /// Payload carried by the transaction (e.g. an encoded activity instruction).
    pub payload: Vec<u8>,
    /// The funding outpoint to spend.
    pub outpoint: TransactionOutpoint,
    /// The funding outpoint's entry (amount, spk, ...).
    pub entry: UtxoEntry,
    /// Key that signs the input.
    pub keypair: Keypair,
    /// Address the remainder (after fee) is paid back to.
    pub address: &'a Address,
    /// Subnetwork the tx is issued on; a lane subnetwork for activity, else native.
    pub subnetwork_id: SubnetworkId,
    /// Transaction version; [`TX_VERSION_TOCCATA`] or newer for non-native subnetworks.
    pub tx_version: u16,
}

/// Builds one signed activity transaction spending `args.outpoint`, paying the remainder (after a
/// mempool-friendly fee) back to `args.address`, carrying `args.payload` on `args.subnetwork_id`.
pub fn activity_transaction(args: ActivityTx<'_>) -> Transaction {
    // Kaspa-mempool-friendly heuristic: 10 × (input + output + base + payload).
    const FEE_PER_BYTE: u64 = 10;
    const INPUT_SIZE: u64 = 200;
    const OUTPUT_SIZE: u64 = 34;
    const BASE_OVERHEAD: u64 = 1000;
    let fee = FEE_PER_BYTE * (INPUT_SIZE + OUTPUT_SIZE + BASE_OVERHEAD + args.payload.len() as u64);
    assert!(args.entry.amount > fee, "UTXO amount {} too small for fee {}", args.entry.amount, fee);

    let input = TransactionInput::new(args.outpoint, vec![], 0, 1);
    let output =
        TransactionOutput::new(args.entry.amount - fee, pay_to_address_script(args.address));

    sign(
        MutableTransaction::with_entries(
            Transaction::new(
                args.tx_version,
                vec![input],
                vec![output],
                0,
                args.subnetwork_id,
                0,
                args.payload,
            ),
            vec![args.entry],
        ),
        args.keypair,
    )
    .tx
}

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

/// Inputs to [`settlement_transaction`].
pub struct SettlementTx<'a> {
    /// The covenant-spending settlement tx (its input 0 already carries the covenant witness).
    pub settlement_tx: Transaction,
    /// Entry of the covenant UTXO being spent by input 0.
    pub covenant_entry: UtxoEntry,
    /// Compute budget to set on the covenant input.
    pub covenant_compute_budget: ComputeBudget,
    /// Funding outpoint that pays the fee.
    pub fee_outpoint: TransactionOutpoint,
    /// The fee outpoint's entry.
    pub fee_entry: UtxoEntry,
    /// Key that signs the fee input.
    pub keypair: Keypair,
    /// Address the change is paid back to.
    pub address: &'a Address,
    /// Consensus params, for storage mass.
    pub params: &'a Params,
}

/// Funds and signs a settlement transaction without submitting it: appends a fee input + change
/// output, signs only the fee input, preserves the covenant input's witness, sets the covenant
/// input's compute budget, and commits the storage-mass field. The result is ready to submit.
pub fn settlement_transaction(args: SettlementTx<'_>) -> Transaction {
    const FEE: u64 = 100_000;

    // Snapshot the covenant witness before signing - `sign` overwrites all signature scripts.
    let covenant_sig_script = args.settlement_tx.inputs[0].signature_script.clone();
    assert!(args.fee_entry.amount > FEE, "fee UTXO amount {} ≤ fee {FEE}", args.fee_entry.amount);

    let mut tx = args.settlement_tx;
    tx.inputs.push(TransactionInput::new(args.fee_outpoint, vec![], 0, 1));
    tx.outputs.push(TransactionOutput::new(
        args.fee_entry.amount - FEE,
        pay_to_address_script(args.address),
    ));

    let entries = vec![args.covenant_entry, args.fee_entry];
    let signed = sign(MutableTransaction::with_entries(tx, entries.clone()), args.keypair);
    let mut tx = signed.tx;

    // Restore the covenant witness and set the covenant input's compute budget.
    tx.inputs[0].signature_script = covenant_sig_script;
    tx.inputs[0].mass = args.covenant_compute_budget.into();

    // Toccata txs must commit storage mass; compute it now that inputs and outputs are final.
    commit_storage_mass(args.params, &tx, &entries);

    // The signature script on input 0 changed, so recompute the on-the-wire id.
    tx.finalize();
    tx
}
