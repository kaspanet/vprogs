//! L1 transaction construction for the runtime driver.
//!
//! Two shapes the generic wallet does not cover:
//! - **Deposit txs** ([`build_deposit_transaction`]) carry a covenant-paying funding output at a
//!   known index alongside the change output; the guest reads that output's value as the credited
//!   amount and checks its SPK against the config-committed deposit address.
//! - **Signed lane-action txs** ([`build_lane_action_transaction`]) carry an Init/Transfer/Withdraw
//!   payload whose BIP-340 signature commits to the transaction's own `rest_preimage`. Since a v1
//!   `rest_preimage` excludes the payload, the signature scripts, and the mass field, the builder
//!   fee-prices the tx first, derives the rest from the funded skeleton, signs the L2 message over
//!   it, then rebuilds with the real payload (same length, so fee and rest are unchanged).
//!
//! The pure [`deposit_funding_rest_preimage`] is also reused by the direct-guest test to synthesize
//! the funding-output preimage without an L1 node.

use kaspa_consensus_core::{
    config::params::Params,
    constants::TX_VERSION_TOCCATA,
    hashing::tx::transaction_v1_rest_preimage,
    mass::MassCalculator,
    sign::sign,
    subnets::{SUBNETWORK_ID_NATIVE, SubnetworkId},
    tx::{
        MutableTransaction, PopulatedTransaction, Transaction, TransactionInput,
        TransactionOutpoint, TransactionOutput, UtxoEntry,
    },
};
use kaspa_addresses::Address;
use kaspa_txscript::{pay_to_address_script, standard::pay_to_script_hash_script};
use secp256k1::Keypair;
use vprogs_l1_wallet::build::commit_storage_mass;
use vprogs_zk_backend_risc0_api::build_delegate_entry_script;

use crate::actions::{self, TestSigner};

/// mempool floor: a transaction's fee must be at least this many sompi per mass gram.
const MIN_FEERATE_PER_GRAM: u64 = 100;

/// The minimum sompi fee the node's mempool requires for `tx`: [`MIN_FEERATE_PER_GRAM`] times the
/// binding mass (the larger of compute and KIP-0009 storage mass). Mirrors the wallet's private
/// helper; called on the final signed tx so signature scripts are counted.
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

/// Builds the `rest_preimage` of a single-output funding tx paying `value` sompi to the covenant
/// deposit address `P2SH(delegate_entry_script(covenant_id))`. This is what the guest parses for a
/// deposit's funding output; the funding output sits at output index 0.
pub fn deposit_funding_rest_preimage(covenant_id: &[u8; 32], value: u64) -> Vec<u8> {
    let spk = pay_to_script_hash_script(&build_delegate_entry_script(covenant_id));
    let tx = Transaction::new(
        1,
        Vec::new(),
        vec![TransactionOutput::new(value, spk)],
        0,
        SUBNETWORK_ID_NATIVE,
        0,
        Vec::new(),
    );
    transaction_v1_rest_preimage(&tx)
}

/// Inputs to [`build_deposit_transaction`].
pub struct DepositTx<'a> {
    /// The signature-free deposit lane payload (from [`actions::deposit_payload`]) whose Deposit
    /// action cites output index 0.
    pub payload: Vec<u8>,
    /// Covenant the funding output pays into (as `P2SH(delegate_entry_script(covenant_id))`).
    pub covenant_id: [u8; 32],
    /// Sompi credited to the user: the value of the funding output at index 0.
    pub deposit_value: u64,
    /// The funding outpoint to spend.
    pub outpoint: TransactionOutpoint,
    /// The funding outpoint's entry.
    pub entry: UtxoEntry,
    /// Key that signs the input and receives the change.
    pub keypair: Keypair,
    /// Address the change (after the deposit output and fee) is paid back to.
    pub change_address: &'a Address,
    /// Lane subnetwork the tx is issued on.
    pub subnetwork_id: SubnetworkId,
    /// Consensus params, for the mass-based fee and storage mass.
    pub params: &'a Params,
}

/// Builds one signed deposit transaction: output 0 pays `deposit_value` to the covenant deposit
/// address, output 1 returns the change, and `payload` (whose Deposit action cites output 0) rides
/// on the lane subnetwork.
pub fn build_deposit_transaction(args: DepositTx<'_>) -> Transaction {
    let deposit_spk = pay_to_script_hash_script(&build_delegate_entry_script(&args.covenant_id));
    let change_spk = pay_to_address_script(args.change_address);
    let input = TransactionInput::new(args.outpoint, vec![], 0, 1);
    let entries = vec![args.entry.clone()];

    let build = |fee: u64| {
        let outputs = vec![
            TransactionOutput::new(args.deposit_value, deposit_spk.clone()),
            TransactionOutput::new(
                args.entry.amount - args.deposit_value - fee,
                change_spk.clone(),
            ),
        ];
        let tx = Transaction::new(
            TX_VERSION_TOCCATA,
            vec![input.clone()],
            outputs,
            0,
            args.subnetwork_id,
            0,
            args.payload.clone(),
        );
        let signed = sign(MutableTransaction::with_entries(tx, entries.clone()), args.keypair).tx;
        commit_storage_mass(args.params, &signed, &entries);
        signed
    };

    // Zero-fee probe to learn the signed mass, price it at the node floor, then rebuild funded.
    let probe = build(0);
    let fee = min_fee(args.params, &probe, &entries);
    assert!(
        args.entry.amount > args.deposit_value + fee,
        "funding UTXO {} too small for deposit {} + fee {}",
        args.entry.amount,
        args.deposit_value,
        fee,
    );
    build(fee)
}

/// Inputs to [`build_lane_action_transaction`].
pub struct LaneActionTx<'a> {
    /// The action's pre-signature prefix (from `actions::*_presig`): `access_meta || signers ||
    /// actions`. The builder appends the signature over the tx's own rest.
    pub presig: Vec<u8>,
    /// Key that signs the action (genesis key for Init, the user's key otherwise).
    pub signer: &'a TestSigner,
    /// The funding outpoint to spend (pays the fee; the remainder is change).
    pub outpoint: TransactionOutpoint,
    /// The funding outpoint's entry.
    pub entry: UtxoEntry,
    /// Key that signs the L1 input and receives the change.
    pub keypair: Keypair,
    /// Address the change (after fee) is paid back to.
    pub change_address: &'a Address,
    /// Lane subnetwork the tx is issued on.
    pub subnetwork_id: SubnetworkId,
    /// Consensus params, for the mass-based fee and storage mass.
    pub params: &'a Params,
}

/// Builds one signed lane-action transaction carrying an Init/Transfer/Withdraw payload. The action
/// signature commits to this tx's `rest_preimage`, so the builder fee-prices a placeholder-payload
/// skeleton, derives the rest, signs the action over it, then rebuilds with the real payload.
pub fn build_lane_action_transaction(args: LaneActionTx<'_>) -> Transaction {
    let change_spk = pay_to_address_script(args.change_address);
    let input = TransactionInput::new(args.outpoint, vec![], 0, 1);
    let entries = vec![args.entry.clone()];

    // The final payload is exactly `presig.len() + 64` bytes (a 64-byte BIP-340 signature tail); a
    // zero-filled placeholder of that length prices the identical mass.
    let payload_len = args.presig.len() + 64;

    let build = |fee: u64, payload: Vec<u8>| {
        let tx = Transaction::new(
            TX_VERSION_TOCCATA,
            vec![input.clone()],
            vec![TransactionOutput::new(args.entry.amount - fee, change_spk.clone())],
            0,
            args.subnetwork_id,
            0,
            payload,
        );
        let signed = sign(MutableTransaction::with_entries(tx, entries.clone()), args.keypair).tx;
        commit_storage_mass(args.params, &signed, &entries);
        signed
    };

    let placeholder = vec![0u8; payload_len];
    let probe = build(0, placeholder.clone());
    let fee = min_fee(args.params, &probe, &entries);
    assert!(args.entry.amount > fee, "funding UTXO {} too small for fee {}", args.entry.amount, fee);

    // Rest excludes payload/sig-scripts/mass, so this funded skeleton's rest is final.
    let funded_skeleton = build(fee, placeholder);
    let rest = transaction_v1_rest_preimage(&funded_skeleton);
    let real_payload = actions::finish_signed_payload(args.presig, args.signer, &rest);
    debug_assert_eq!(real_payload.len(), payload_len);
    build(fee, real_payload)
}
