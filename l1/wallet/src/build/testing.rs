//! Shared fixtures for the builder regression tests: deterministic keys/addresses/outpoints, the
//! node-mirrored fee/mass math the tests check builder pricing against, and the concrete shapes
//! (settlement, payout, activity) the fee-pricing and funding-search regressions build through.
//!
//! Every builder prices its fee from a signed zero-fee probe. The fee floor mirrors the node's
//! post-Toccata admission rule (the mempool relay rate over the larger of compute mass and
//! normalized transient mass), both of which depend only on the byte layout the fee value
//! cannot change, so the probe prices the final transaction exactly. Storage mass never enters
//! the fee; it bounds validity through the block-fit limit, which the funding loop keeps by
//! requiring a storage-viable change output, dropping the change, or adding inputs.

use kaspa_addresses::{Address, Prefix, Version};
use kaspa_consensus_core::{
    config::params::Params,
    constants::TX_VERSION_TOCCATA,
    mass::{MassCalculator, units::ComputeBudget},
    sign::sign,
    subnets::SUBNETWORK_ID_NATIVE,
    tx::{
        CovenantBinding, MutableTransaction, PopulatedTransaction, Transaction, TransactionInput,
        TransactionOutpoint, TransactionOutput, UtxoEntry,
    },
};
use kaspa_hashes::Hash;
use kaspa_txscript::{pay_to_address_script, standard::pay_to_script_hash_script};
use secp256k1::Keypair;

use super::{
    activity::{ActivityTx, activity_transaction},
    funding::fee_paid,
    payout::{PayToAddressTx, pay_to_address_transaction},
    pricing::{FeePolicy, min_fee},
    settlement::{SettlementTx, settlement_transaction},
};

/// The value `Wallet::pay_to_address` is asked for when seeding a prover's fee address in
/// `examples/tn10-flow/tests/two_provers_contend.rs`, i.e. the size of a real fee UTXO.
pub(super) const FUND_VALUE: u64 = 100_000_000;

/// `zk::backend::risc0::covenant::script::DEFAULT_PERMISSION_OUTPUT_VALUE`, the value a real
/// settlement pays to its permission output.
pub(super) const PERMISSION_OUTPUT_VALUE: u64 = 50_000_000;

/// `sim::driver::l2_driver::DEV_COVENANT_BUDGET`, the budget a dev-mode settlement commits on
/// its covenant input.
pub(super) const DEV_COVENANT_BUDGET: ComputeBudget = ComputeBudget(100);

pub(super) fn keypair() -> Keypair {
    Keypair::from_seckey_slice(secp256k1::SECP256K1, &[7u8; 32])
        .expect("static secret key is valid")
}

pub(super) fn address(keypair: &Keypair, params: &Params) -> Address {
    let (xonly, _parity) = keypair.x_only_public_key();
    Address::new(Prefix::from(params.net.network_type()), Version::PubKey, &xonly.serialize())
}

pub(super) fn outpoint(seed: u8) -> TransactionOutpoint {
    TransactionOutpoint::new(Hash::from_bytes([seed; 32]), 0)
}

/// The mempool's minimum relay fee, in sompi per 1000 grams of mass, from rusty-kaspa's
/// post-Toccata `DEFAULT_MINIMUM_RELAY_TRANSACTION_FEE`. Mirrored here because the node's
/// mempool config is crate-private and this crate does not depend on `kaspa-mining`.
pub(super) const MEMPOOL_MIN_RELAY_FEE_PER_KILOGRAM: u64 = 100_000;

/// The sompi fee a default-configured node's mempool requires for `tx` once Toccata is
/// active: the relay fee rate over the larger of compute mass and normalized transient
/// mass. Storage mass never enters the node's fee floor.
pub(super) fn mempool_min_fee(params: &Params, tx: &Transaction) -> u64 {
    let calc = MassCalculator::new(
        params.mass_per_tx_byte,
        params.mass_per_script_pub_key_byte,
        params.storage_mass_parameter,
    );
    let masses = calc.calc_non_contextual_masses(tx);
    let cofactors = params.block_mass_limits().raw_post().cofactors();
    let fee_mass = masses.compute_mass.max(masses.normalized_transient(&cofactors));
    (fee_mass * MEMPOOL_MIN_RELAY_FEE_PER_KILOGRAM) / 1000
}

/// The node's priority mass for `tx`: the feerate-ordering mass from the mempool's
/// feerate key, the normalized max of compute, transient, and storage mass. Recomputed
/// from the cofactors here rather than through the production pricing path, so the two
/// sides check each other.
pub(super) fn priority_mass_mirror(
    params: &Params,
    tx: &Transaction,
    entries: &[UtxoEntry],
) -> u64 {
    let calc = MassCalculator::new(
        params.mass_per_tx_byte,
        params.mass_per_script_pub_key_byte,
        params.storage_mass_parameter,
    );
    let masses = calc.calc_non_contextual_masses(tx);
    let populated = PopulatedTransaction::new(tx, entries.to_vec());
    let storage = calc
        .calc_contextual_masses(&populated)
        .expect("contextual mass calculation must succeed for a populated transaction")
        .storage_mass;
    let cofactors = params.block_mass_limits().raw_post().cofactors();
    let storage_normalized = (storage as f64 * cofactors.storage).ceil() as u64;
    masses.compute_mass.max(masses.normalized_transient(&cofactors)).max(storage_normalized)
}

/// The least fee paying `rate` sompi per gram of `tx`'s priority mass without dropping
/// below the mempool floor.
pub(super) fn target_fee_mirror(
    params: &Params,
    rate: f64,
    tx: &Transaction,
    entries: &[UtxoEntry],
) -> u64 {
    let target = (rate * priority_mass_mirror(params, tx, entries) as f64).ceil() as u64;
    mempool_min_fee(params, tx).max(target)
}

/// Fails when `tx` pays less than `min_fee` asks for `tx` itself, or when its committed
/// storage mass is above the block-fit limit.
///
/// The fee check is the builders' own pricing policy, not the node's admission rule; see
/// `built_shapes_meet_the_mempool_floor`. Reports every mass term so a failure shows which
/// one binds.
pub(super) fn assert_fee_covers_final(params: &Params, tx: &Transaction, entries: &[UtxoEntry]) {
    let calc = MassCalculator::new(
        params.mass_per_tx_byte,
        params.mass_per_script_pub_key_byte,
        params.storage_mass_parameter,
    );
    let masses = calc.calc_non_contextual_masses(tx);
    let compute = masses.compute_mass;
    let cofactors = params.block_mass_limits().raw_post().cofactors();
    let transient = masses.normalized_transient(&cofactors);
    let populated = PopulatedTransaction::new(tx, entries.to_vec());
    let storage = calc.calc_contextual_masses(&populated).map_or(0, |m| m.storage_mass);
    let paid = fee_paid(tx, entries);
    let required = min_fee(params, tx);
    assert!(
        paid >= required,
        "built tx pays {paid} but its own min_fee is {required} \
         (compute mass {compute}, normalized transient mass {transient}, storage mass {storage})",
    );
    let limit = params.block_mass_limits().raw_post().storage;
    assert!(
        storage <= limit,
        "built tx carries storage mass {storage}, above the block-fit limit {limit}",
    );
}

/// A settlement skeleton with `witness_len` bytes of covenant witness on input 0, a
/// continuation covenant output, and the permission output. Stands in for what the settler
/// hands `settlement_transaction`; only its size and output values reach the mass
/// calculation.
pub(super) fn settlement_skeleton(
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

/// A transaction a builder produced, with the entries its inputs spend.
pub(super) struct BuiltShape {
    pub(super) tx: Transaction,
    pub(super) entries: Vec<UtxoEntry>,
}

/// A dev-mode settlement, built through `settlement_transaction`.
///
/// Every value here is one the repo already uses: a `FUND_VALUE` fee UTXO, the default
/// permission output value, and the dev covenant budget. Only the witness length is a
/// stand-in for compute mass.
pub(super) fn settlement_shape(params: &Params) -> BuiltShape {
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
        fee_candidates: vec![(outpoint(3), fee_entry.clone())],
        keypair,
        address: &address,
        fee_policy: FeePolicy::Floor,
        params,
    })
    .expect("settlement is fundable");

    BuiltShape { tx, entries: vec![covenant_entry, fee_entry] }
}

/// A payout leaving change small enough that its `C/v` term dominates compute mass, built
/// through `pay_to_address_transaction`.
///
/// The change value here is smaller than the coinbase-funded change the repo's own callers
/// leave, so this shape is reachable through the public builder but is not what `Wallet`
/// currently produces.
pub(super) fn pay_to_address_shape(params: &Params) -> BuiltShape {
    let keypair = keypair();
    let address = address(&keypair, params);
    let payout = FUND_VALUE;
    let change = 50_000_000;
    let entry = UtxoEntry::new(payout + change, pay_to_address_script(&address), 0, false, None);

    let tx = pay_to_address_transaction(PayToAddressTx {
        candidates: vec![(outpoint(1), entry.clone())],
        recipient: &address,
        value: payout,
        count: 1,
        keypair,
        change_address: &address,
        params,
    })
    .expect("payout is fundable");

    BuiltShape { tx, entries: vec![entry] }
}

/// An activity transaction whose funding UTXO is small enough that the `C/v` term on its single
/// output dominates compute mass, built through `activity_transaction`.
///
/// 0.1 KAS is around the smallest output KIP-0009 keeps relayable, and `Wallet` spends its
/// largest UTXO first, so this shape is reachable through the public builder (which the
/// simulation calls directly with its own UTXOs) but is not one `Wallet` currently selects.
pub(super) fn activity_shape(params: &Params) -> BuiltShape {
    let keypair = keypair();
    let address = address(&keypair, params);
    let entry = UtxoEntry::new(10_000_000, pay_to_address_script(&address), 0, false, None);

    let tx = activity_transaction(ActivityTx {
        payload: vec![0u8; 64],
        candidates: vec![(outpoint(1), entry.clone())],
        keypair,
        address: &address,
        subnetwork_id: SUBNETWORK_ID_NATIVE,
        tx_version: TX_VERSION_TOCCATA,
        fee_policy: FeePolicy::Floor,
        params,
    })
    .expect("activity is fundable");

    BuiltShape { tx, entries: vec![entry] }
}

/// Candidate entries of the given amounts, all P2PK to `address`, with distinct outpoints.
pub(super) fn candidates_of(
    amounts: &[u64],
    address: &Address,
) -> Vec<(TransactionOutpoint, UtxoEntry)> {
    amounts
        .iter()
        .enumerate()
        .map(|(i, &amount)| {
            (
                outpoint(i as u8 + 1),
                UtxoEntry::new(amount, pay_to_address_script(address), 0, false, None),
            )
        })
        .collect()
}

/// A `fund` probe closure for a layout with one fixed `payout` output and an optional
/// trailing change output, spending the first `n` candidates.
pub(super) fn payout_probe<'a>(
    candidates: &'a [(TransactionOutpoint, UtxoEntry)],
    payout: u64,
    spk: &'a kaspa_consensus_core::tx::ScriptPublicKey,
    keypair: &'a Keypair,
) -> impl FnMut(usize, Option<u64>) -> (Transaction, Vec<UtxoEntry>) + 'a {
    let keypair = *keypair;
    move |n, change| {
        let inputs =
            candidates[..n].iter().map(|(o, _)| TransactionInput::new(*o, vec![], 0, 1)).collect();
        let mut outputs = vec![TransactionOutput::new(payout, spk.clone())];
        outputs.extend(change.map(|v| TransactionOutput::new(v, spk.clone())));
        let tx = Transaction::new(
            TX_VERSION_TOCCATA,
            inputs,
            outputs,
            0,
            SUBNETWORK_ID_NATIVE,
            0,
            Vec::new(),
        );
        let entries: Vec<UtxoEntry> = candidates[..n].iter().map(|(_, e)| e.clone()).collect();
        (sign(MutableTransaction::with_entries(tx, entries.clone()), keypair).tx, entries)
    }
}
