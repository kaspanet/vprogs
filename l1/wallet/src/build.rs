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
    hashing::{covenant_id::covenant_id, tx::transaction_v1_rest_preimage},
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

/// mempool floor: a transaction's fee must be at least this many sompi per mass gram.
const MIN_FEERATE_PER_GRAM: u64 = 100;

/// The minimum sompi fee the node's mempool requires for `tx`: [`MIN_FEERATE_PER_GRAM`] times the
/// binding mass: the larger of the tx's compute mass and its KIP-0009 storage mass.
///
/// Call this on the *final, signed* tx so the signature scripts are counted (an unsigned tx has
/// empty sig scripts and undercounts compute mass). The fee value itself doesn't change the byte
/// layout, so a zero-fee probe yields the same compute mass; storage mass is ~0 for the simple
/// shapes here, so it stays compute-dominated.
fn min_fee(params: &Params, tx: &Transaction, entries: &[UtxoEntry]) -> u64 {
    let calc = MassCalculator::new(
        params.mass_per_tx_byte,
        params.mass_per_script_pub_key_byte,
        params.storage_mass_parameter,
    );
    let compute = calc.calc_non_contextual_masses(tx).compute_mass;
    let populated = PopulatedTransaction::new(tx, entries.to_vec());
    let storage = calc.calc_contextual_masses(&populated).map_or(0, |m| m.storage_mass);
    MIN_FEERATE_PER_GRAM * compute.max(storage)
}

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
    tx.set_storage_mass(masses.storage_mass);
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
    /// Consensus params, for the mass-based fee.
    pub params: &'a Params,
}

/// Builds one signed activity transaction spending `args.outpoint`, paying the remainder (after the
/// node's minimum mass-based fee) back to `args.address`, carrying `args.payload` on
/// `args.subnetwork_id`.
pub fn activity_transaction(args: ActivityTx<'_>) -> Transaction {
    let input = TransactionInput::new(args.outpoint, vec![], 0, 1);
    let spk = pay_to_address_script(args.address);
    let entries = vec![args.entry.clone()];
    let build = |out_amount: u64| {
        Transaction::new(
            args.tx_version,
            vec![input.clone()],
            vec![TransactionOutput::new(out_amount, spk.clone())],
            0,
            args.subnetwork_id,
            0,
            args.payload.clone(),
        )
    };

    // Sign a zero-fee probe to learn the signed tx's mass (the fee amount doesn't change the byte
    // layout), price it at the node's floor, then re-sign with the funded output.
    let probe = sign(
        MutableTransaction::with_entries(build(args.entry.amount), entries.clone()),
        args.keypair,
    )
    .tx;
    let fee = min_fee(args.params, &probe, &entries);
    assert!(args.entry.amount > fee, "UTXO amount {} too small for fee {}", args.entry.amount, fee);

    sign(MutableTransaction::with_entries(build(args.entry.amount - fee), entries), args.keypair).tx
}

/// Inputs to [`pay_to_address_transaction`].
pub struct PayToAddressTx<'a> {
    /// The funding outpoint to spend.
    pub outpoint: TransactionOutpoint,
    /// The funding outpoint's entry (amount, spk, ...).
    pub entry: UtxoEntry,
    /// Recipient address each of the `count` outputs pays.
    pub recipient: &'a Address,
    /// Sompi paid to each recipient output.
    pub value: u64,
    /// Number of equal-value outputs to the recipient.
    pub count: usize,
    /// Key that funds the input and receives the change.
    pub keypair: Keypair,
    /// Address the remainder (after the recipient outputs and the fee) is paid back to.
    pub change_address: &'a Address,
    /// Consensus params, for the mass-based fee and storage mass.
    pub params: &'a Params,
}

/// Builds one signed transaction spending `args.outpoint` that pays `args.count` outputs of
/// `args.value` sompi each to `args.recipient`, returning the remainder (after those outputs and
/// the node's minimum mass-based fee) as change to `args.change_address`. Used to seed a distinct
/// prover's funding address from a coinbase wallet.
pub fn pay_to_address_transaction(args: PayToAddressTx<'_>) -> Transaction {
    let input = TransactionInput::new(args.outpoint, vec![], 0, 1);
    let recipient_spk = pay_to_address_script(args.recipient);
    let change_spk = pay_to_address_script(args.change_address);
    let payout: u64 = args.value.checked_mul(args.count as u64).expect("recipient payout overflow");
    assert!(
        args.entry.amount > payout,
        "funding UTXO amount {} too small for payout {}",
        args.entry.amount,
        payout,
    );
    let entries = vec![args.entry.clone()];

    let build = |fee: u64| {
        let mut outputs: Vec<TransactionOutput> = (0..args.count)
            .map(|_| TransactionOutput::new(args.value, recipient_spk.clone()))
            .collect();
        outputs.push(TransactionOutput::new(args.entry.amount - payout - fee, change_spk.clone()));
        let tx = Transaction::new(
            TX_VERSION_TOCCATA,
            vec![input.clone()],
            outputs,
            0,
            SUBNETWORK_ID_NATIVE,
            0,
            Vec::new(),
        );
        let signed = sign(MutableTransaction::with_entries(tx, entries.clone()), args.keypair).tx;
        commit_storage_mass(args.params, &signed, &entries);
        signed
    };

    // Zero-fee probe to learn the signed mass, price it at the node floor, then rebuild funded.
    let probe = build(0);
    let fee = min_fee(args.params, &probe, &entries);
    assert!(
        args.entry.amount > payout + fee,
        "funding UTXO amount {} too small for payout {} + fee {}",
        args.entry.amount,
        payout,
        fee,
    );
    build(fee)
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
    /// Transaction version; [`TX_VERSION_TOCCATA`] or newer for non-native subnetworks.
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
    let fee = min_fee(args.params, &probe, &entries);
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
    // Snapshot the covenant witness before signing - `sign` overwrites all signature scripts.
    let covenant_sig_script = args.settlement_tx.inputs[0].signature_script.clone();
    let change_spk = pay_to_address_script(args.address);
    let entries = vec![args.covenant_entry.clone(), args.fee_entry.clone()];

    // Build the fully-funded, signed tx for a given fee: append the fee input + change output, sign
    // only the fee input, restore the covenant witness, set its compute budget, and commit storage
    // mass. The succinct witness makes this tx large, so its fee is mass-dominated; well past the
    // old flat 100_000.
    let build = |fee: u64| {
        let mut tx = args.settlement_tx.clone();
        tx.inputs.push(TransactionInput::new(args.fee_outpoint, vec![], 0, 1));
        tx.outputs.push(TransactionOutput::with_covenant(
            args.fee_entry.amount - fee,
            change_spk.clone(),
            None,
        ));
        let mut tx = sign(MutableTransaction::with_entries(tx, entries.clone()), args.keypair).tx;
        tx.inputs[0].signature_script = covenant_sig_script.clone();
        tx.inputs[0].compute_commit = args.covenant_compute_budget.into();
        commit_storage_mass(args.params, &tx, &entries);
        // The signature script on input 0 changed, so recompute the on-the-wire id.
        tx.finalize();
        tx
    };

    // Probe at fee 0 to learn the mass, price it at the node's floor, then rebuild funded.
    let probe = build(0);
    let fee = min_fee(args.params, &probe, &entries);
    assert!(args.fee_entry.amount > fee, "fee UTXO amount {} ≤ fee {fee}", args.fee_entry.amount);
    build(fee)
}
