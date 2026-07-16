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

/// Repros for the single-round fee probe.
///
/// Every builder in this file prices its fee from a `fee = 0` probe and rebuilds once, without
/// re-pricing the rebuilt transaction. Paying the fee shrinks the change output, and under KIP-0009
/// a smaller output carries *more* storage mass (the `C/v` term). When storage mass is the binding
/// mass, the rebuilt transaction therefore needs a strictly larger fee than the probe was priced
/// for, and the builder emits a transaction the mempool's own floor rejects. The `assert!` in each
/// builder cannot catch this: it prices the probe, not the result.
///
/// Each test below runs a builder and re-prices its output with the same [`min_fee`] the builder
/// used, asserting the built transaction pays at least what it now costs.
#[cfg(test)]
mod tests {
    use kaspa_addresses::{Prefix, Version};
    use kaspa_consensus_core::config::params::SIMNET_PARAMS;

    use super::*;

    /// The value `Wallet::pay_to_address` is asked for when seeding a prover's fee address in
    /// `examples/tn10-flow/tests/two_provers_contend.rs`, i.e. the size of a real fee UTXO.
    const FUND_VALUE: u64 = 100_000_000;

    /// `zk::backend::risc0::covenant::script::DEFAULT_PERMISSION_OUTPUT_VALUE`, the value a real
    /// settlement pays to its permission output.
    const PERMISSION_OUTPUT_VALUE: u64 = 50_000_000;

    /// `sim::driver::l2_driver::DEV_COVENANT_BUDGET`, the budget a dev-mode settlement commits on
    /// its covenant input.
    const DEV_COVENANT_BUDGET: ComputeBudget = ComputeBudget(100);

    fn keypair() -> Keypair {
        Keypair::from_seckey_slice(secp256k1::SECP256K1, &[7u8; 32])
            .expect("static secret key is valid")
    }

    fn address(keypair: &Keypair, params: &Params) -> Address {
        let (xonly, _parity) = keypair.x_only_public_key();
        Address::new(Prefix::from(params.net.network_type()), Version::PubKey, &xonly.serialize())
    }

    fn outpoint(seed: u8) -> TransactionOutpoint {
        TransactionOutpoint::new(Hash::from_bytes([seed; 32]), 0)
    }

    /// The fee `tx` actually pays: what its inputs bring in, less what its outputs pay out.
    fn fee_paid(tx: &Transaction, entries: &[UtxoEntry]) -> u64 {
        entries.iter().map(|entry| entry.amount).sum::<u64>()
            - tx.outputs.iter().map(|output| output.value).sum::<u64>()
    }

    /// Fails when `tx` pays less than the node's floor for `tx` itself, which is exactly what the
    /// mempool checks on submit. Reports the binding mass so a failure shows which term binds.
    fn assert_fee_covers_final(params: &Params, tx: &Transaction, entries: &[UtxoEntry]) {
        let calc = MassCalculator::new(
            params.mass_per_tx_byte,
            params.mass_per_script_pub_key_byte,
            params.storage_mass_parameter,
        );
        let compute = calc.calc_non_contextual_masses(tx).compute_mass;
        let populated = PopulatedTransaction::new(tx, entries.to_vec());
        let storage = calc.calc_contextual_masses(&populated).map_or(0, |m| m.storage_mass);
        let paid = fee_paid(tx, entries);
        let required = min_fee(params, tx, entries);
        assert!(
            paid >= required,
            "built tx pays {paid} but its own min_fee is {required} \
             (compute mass {compute}, storage mass {storage})",
        );
    }

    /// A settlement skeleton with `witness_len` bytes of covenant witness on input 0, a
    /// continuation covenant output, and the permission output. Stands in for what the settler
    /// hands [`settlement_transaction`]; only its size and output values reach the mass
    /// calculation.
    fn settlement_skeleton(
        covenant_spk: &kaspa_consensus_core::tx::ScriptPublicKey,
        covenant_id: Hash,
        continuation_value: u64,
        recipient: &Address,
        witness_len: usize,
    ) -> Transaction {
        let mut tx = Transaction::new(
            TX_VERSION_TOCCATA,
            vec![TransactionInput::new(outpoint(2), vec![0x51u8; witness_len], 0, 1)],
            vec![
                TransactionOutput::with_covenant(
                    continuation_value,
                    covenant_spk.clone(),
                    Some(CovenantBinding::new(0, covenant_id)),
                ),
                TransactionOutput::new(PERMISSION_OUTPUT_VALUE, pay_to_address_script(recipient)),
            ],
            0,
            SUBNETWORK_ID_NATIVE,
            0,
            Vec::new(),
        );
        tx.finalize();
        tx
    }

    /// A dev-mode settlement is storage-mass-dominated, so [`settlement_transaction`] underprices
    /// it: its own covenant and permission outputs put storage mass far above compute mass, and
    /// deducting the probe-priced fee shrinks the change output enough to raise storage mass past
    /// what was paid.
    ///
    /// Every value here is one the repo already uses: a `FUND_VALUE` fee UTXO, the default
    /// permission output value, and the dev covenant budget. Only the witness length is a stand-in;
    /// it sets compute mass, and a real dev witness must merely stay under the storage mass this
    /// shape already carries for the underpay to hold.
    ///
    /// This refutes [`min_fee`]'s claim that "storage mass is ~0 for the simple shapes here".
    #[test]
    #[ignore = "repro: the settlement fee is priced from a fee=0 probe, so deducting it shrinks the \
                change output and raises the storage mass past the fee that was paid"]
    fn settlement_fee_must_cover_the_final_transactions_storage_mass() {
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
                2_000,
            ),
            covenant_entry: covenant_entry.clone(),
            covenant_compute_budget: DEV_COVENANT_BUDGET,
            fee_outpoint: outpoint(3),
            fee_entry: fee_entry.clone(),
            keypair,
            address: &address,
            params,
        });

        assert_fee_covers_final(params, &tx, &[covenant_entry, fee_entry]);
    }

    /// [`pay_to_address_transaction`] underprices whenever the change left over above the payout is
    /// small enough that its `C/v` term dominates compute mass.
    ///
    /// The change value here is smaller than the coinbase-funded change the repo's own callers
    /// leave, so this shape is reachable through the public builder but is not what `Wallet`
    /// currently produces.
    #[test]
    #[ignore = "repro: the payout fee is priced from a fee=0 probe, so deducting it shrinks the \
                change output and raises the storage mass past the fee that was paid"]
    fn pay_to_address_fee_must_cover_the_final_changes_storage_mass() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let payout = FUND_VALUE;
        let change = 50_000_000;
        let entry =
            UtxoEntry::new(payout + change, pay_to_address_script(&address), 0, false, None);

        let tx = pay_to_address_transaction(PayToAddressTx {
            outpoint: outpoint(1),
            entry: entry.clone(),
            recipient: &address,
            value: payout,
            count: 1,
            keypair,
            change_address: &address,
            params,
        });

        assert_fee_covers_final(params, &tx, &[entry]);
    }

    /// [`activity_transaction`] underprices once the funding UTXO is small enough that the `C/v`
    /// term on the single output dominates compute mass. Its probe pays the whole input back out,
    /// so the probe's storage mass is exactly zero and the fee is always priced on compute alone.
    ///
    /// 0.1 KAS is around the smallest output KIP-0009 keeps relayable, and `Wallet` spends its
    /// largest UTXO first, so this shape is reachable through the public builder (which the
    /// simulation calls directly with its own UTXOs) but is not one `Wallet` currently selects.
    #[test]
    #[ignore = "repro: the activity fee is priced from a fee=0 probe whose storage mass is zero, so \
                deducting the fee raises the final output's storage mass past the fee that was paid"]
    fn activity_fee_must_cover_the_final_outputs_storage_mass() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let entry = UtxoEntry::new(10_000_000, pay_to_address_script(&address), 0, false, None);

        let tx = activity_transaction(ActivityTx {
            payload: vec![0u8; 64],
            outpoint: outpoint(1),
            entry: entry.clone(),
            keypair,
            address: &address,
            subnetwork_id: SUBNETWORK_ID_NATIVE,
            tx_version: TX_VERSION_TOCCATA,
            params,
        });

        assert_fee_covers_final(params, &tx, &[entry]);
    }
}
