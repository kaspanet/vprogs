//! Carrier transaction composition and submission helpers.
//!
//! Bridges [`LanePayload`] assembly with [`vprogs_l1_wallet::build::signed_carrier_transaction`]
//! for single-UTXO funded carrier transactions (deposits and lane actions), and provides
//! [`fund_and_submit`] with transient-rejection retry loops.

use std::{cell::RefCell, time::Duration};

use kaspa_addresses::Address;
use kaspa_consensus_core::{
    config::params::Params,
    subnets::SubnetworkId,
    tx::{Transaction, TransactionOutpoint, TransactionOutput, UtxoEntry},
};
use kaspa_hashes::Hash;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_txscript::standard::pay_to_script_hash_script;
use secp256k1::Keypair;
use vprogs_l1_wallet::{
    Wallet,
    build::{SignedCarrierTx, signed_carrier_transaction},
};
use vprogs_zk_backend_risc0_api::build_delegate_entry_script;

use crate::payload::{LanePayload, SigRequest};

/// Default maximum submit attempts for transient rejections.
pub const DEFAULT_MAX_SUBMIT_ATTEMPTS: u32 = 8;
/// Default back-off duration between retry attempts.
pub const DEFAULT_SUBMIT_RETRY_DELAY: Duration = Duration::from_millis(2000);

/// Common transaction arguments for building a signed carrier.
pub struct CarrierTxArgs<'a> {
    /// The funding outpoint to spend.
    pub outpoint: TransactionOutpoint,
    /// The funding outpoint's UTXO entry.
    pub entry: UtxoEntry,
    /// Keypair that signs and funds the input.
    pub keypair: Keypair,
    /// Address to which change (after extra outputs and fee) is paid.
    pub change_address: &'a Address,
    /// Subnetwork id the carrier transaction rides on.
    pub subnetwork_id: SubnetworkId,
    /// Transaction version (e.g. `TX_VERSION_TOCCATA`).
    pub tx_version: u16,
    /// Consensus parameters for fee and storage mass calculation.
    pub params: &'a Params,
    /// Extra outputs prepended before the change output (e.g. deposit outputs).
    pub extra_outputs: Vec<TransactionOutput>,
}

/// Builds one covenant deposit output: `P2SH(delegate_entry_script(covenant_id))`.
///
/// This matches the script commit produced by the guest's `DepositPolicy::deposit_spk`.
/// Deposit transactions place this as output 0; the Deposit action cites `output_idx = 0`.
pub fn covenant_deposit_output(covenant_id: &[u8; 32], value: u64) -> TransactionOutput {
    let spk = pay_to_script_hash_script(&build_delegate_entry_script(covenant_id));
    TransactionOutput::new(value, spk)
}

/// Builds one signed lane-action carrier whose payload is finalized over the carrier's
/// `rest_preimage`.
///
/// The transaction is fee-priced using a fixed-size probe, outputs are frozen, and the real
/// payload is signed over the finalized `rest_preimage` before L1 input signing.
pub fn signed_lane_action_tx(
    args: CarrierTxArgs<'_>,
    payload: &LanePayload,
    sign: &mut dyn FnMut(SigRequest<'_>) -> [u8; 64],
) -> Transaction {
    let sign_cell = RefCell::new(sign);
    signed_carrier_transaction(SignedCarrierTx {
        outpoint: args.outpoint,
        entry: args.entry,
        keypair: args.keypair,
        change_address: args.change_address,
        subnetwork_id: args.subnetwork_id,
        tx_version: args.tx_version,
        params: args.params,
        extra_outputs: args.extra_outputs,
        finalize_payload: |rest: &[u8]| payload.finish(rest, &mut *sign_cell.borrow_mut()),
    })
}

/// Builds one signed deposit carrier: extra outputs are placed first, followed by change.
///
/// The payload is signature-free, evaluated via [`LanePayload::finish_unsigned`].
pub fn signed_deposit_tx(args: CarrierTxArgs<'_>, payload: &LanePayload) -> Transaction {
    signed_carrier_transaction(SignedCarrierTx {
        outpoint: args.outpoint,
        entry: args.entry,
        keypair: args.keypair,
        change_address: args.change_address,
        subnetwork_id: args.subnetwork_id,
        tx_version: args.tx_version,
        params: args.params,
        extra_outputs: args.extra_outputs,
        finalize_payload: |_: &[u8]| payload.finish_unsigned(),
    })
}

/// Determines whether an RPC submit rejection is transient (mempool orphan or timeout).
pub fn is_transient_submit_error(err: &impl std::fmt::Display) -> bool {
    let msg = err.to_string();
    msg.contains("orphan") || msg.contains("timed out") || msg.contains("timeout")
}

/// Fetches spendable UTXOs, builds a carrier via `build`, and submits with retry back-off.
///
/// Re-fetches spendable UTXOs on transient rejections to adapt to mined UTXOs, returning
/// `Some(tx_id)` on acceptance or `None` if funding fails or retries are exhausted.
pub async fn fund_and_submit<C: RpcApi + ?Sized>(
    label: &str,
    wallet: &Wallet<'_, C>,
    build: impl Fn(Vec<(TransactionOutpoint, UtxoEntry)>) -> Option<Transaction>,
    attempts: u32,
    retry_delay: Duration,
) -> Option<Hash> {
    for attempt in 1..=attempts {
        let candidates = match wallet.fetch_spendable_utxos().await {
            Ok(utxos) => utxos,
            Err(e) => {
                log::warn!("{label}: spendable-utxo fetch failed: {e}");
                return None;
            }
        };
        if candidates.is_empty() {
            return None;
        }

        let tx = build(candidates)?;
        match wallet.submit_transaction(&tx).await {
            Ok(id) => return Some(id),
            Err(e) if is_transient_submit_error(&e) && attempt < attempts => {
                log::warn!("{label} rejected (attempt {attempt}/{attempts}, retrying): {e}");
                tokio::time::sleep(retry_delay).await;
            }
            Err(e) => {
                log::warn!("{label} not submitted: {e}");
                return None;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use kaspa_addresses::{Prefix, Version};
    use kaspa_consensus_core::{
        constants::TX_VERSION_TOCCATA,
        hashing::tx::transaction_v1_rest_preimage,
        network::{NetworkId, NetworkType},
        subnets::SUBNETWORK_ID_NATIVE,
    };
    use kaspa_txscript::pay_to_address_script;
    use secp256k1::{Message, SECP256K1, SecretKey};
    use vprogs_core_types::{AccessMetadata, AccessType};
    use vprogs_l1_wallet::build::min_fee;
    use vprogs_zk_backend_risc0_runtime_processor::{
        runtime::compute_sig_message, signer_trait::Signer, signer_variants::SchnorrSigPtrSigner,
    };

    use super::*;
    use crate::signer::{Bip340Signer, SignerKind, SignerSpec, TailBlock};

    #[test]
    fn test_deposit_fee_clears_min_on_shrunk_change() {
        let params = Params::from(NetworkId::with_suffix(NetworkType::Testnet, 10));
        let mut secret = [0u8; 32];
        secret[31] = 9;
        let keypair = Keypair::from_secret_key(SECP256K1, &SecretKey::from_slice(&secret).unwrap());
        let change_address = Address::new(
            Prefix::Testnet,
            Version::PubKey,
            &keypair.x_only_public_key().0.serialize(),
        );
        let covenant_id = [7u8; 32];
        let deposit_value = 100_000_000u64;
        let funding = 150_000_000u64;

        let entry = UtxoEntry::new(funding, pay_to_address_script(&change_address), 0, false, None);
        let outpoint = TransactionOutpoint::new(Hash::from_bytes([1u8; 32]), 0);

        let user_id = [0xaa; 32];
        let config_id = [0xcc; 32];
        let deposit_action = vec![0x05, 0x00, 0x00, 0x00, 0x00, 0x00];
        let payload = LanePayload::new()
            .access(AccessMetadata { resource_id: user_id.into(), access_type: AccessType::Write })
            .access(AccessMetadata { resource_id: config_id.into(), access_type: AccessType::Read })
            .action(deposit_action);

        let deposit_output = covenant_deposit_output(&covenant_id, deposit_value);

        let tx = signed_deposit_tx(
            CarrierTxArgs {
                outpoint,
                entry: entry.clone(),
                keypair,
                change_address: &change_address,
                subnetwork_id: SUBNETWORK_ID_NATIVE,
                tx_version: TX_VERSION_TOCCATA,
                params: &params,
                extra_outputs: vec![deposit_output],
            },
            &payload,
        );

        assert_eq!(tx.outputs[0].value, deposit_value);
        let change = tx.outputs[1].value;
        let fee_paid = funding - deposit_value - change;
        let required = min_fee(&params, &tx);
        assert!(fee_paid >= required, "deposit underpaid: fee {fee_paid} < required {required}");
    }

    #[test]
    fn test_signed_lane_action_tx_splices_valid_sig() {
        let params = Params::from(NetworkId::with_suffix(NetworkType::Testnet, 10));
        let mut secret = [0u8; 32];
        secret[31] = 5;
        let keypair = Keypair::from_secret_key(SECP256K1, &SecretKey::from_slice(&secret).unwrap());
        let change_address = Address::new(
            Prefix::Testnet,
            Version::PubKey,
            &keypair.x_only_public_key().0.serialize(),
        );

        let signer = Bip340Signer::new();
        let funding = 50_000_000u64;
        let entry = UtxoEntry::new(funding, pay_to_address_script(&change_address), 0, false, None);
        let outpoint = TransactionOutpoint::new(Hash::from_bytes([2u8; 32]), 1);

        let res0 = [0x11u8; 32];
        let res1 = [0x22u8; 32];
        let action = vec![0x03, 0x00, 0x01, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];

        let payload = LanePayload::new()
            .access(AccessMetadata { resource_id: res0.into(), access_type: AccessType::Write })
            .access(AccessMetadata { resource_id: res1.into(), access_type: AccessType::Write })
            .action(action)
            .signer(SignerSpec {
                resource_idx: 0,
                kind: SignerKind::SigPtr { tag: SchnorrSigPtrSigner::TAG },
                tail: TailBlock::Sig64,
            });

        let tx = signed_lane_action_tx(
            CarrierTxArgs {
                outpoint,
                entry: entry.clone(),
                keypair,
                change_address: &change_address,
                subnetwork_id: SUBNETWORK_ID_NATIVE,
                tx_version: TX_VERSION_TOCCATA,
                params: &params,
                extra_outputs: vec![],
            },
            &payload,
            &mut |req| signer.sign_digest(req.digest),
        );

        // Verify the payload in the transaction is finalized with a signature committing to the tx
        // rest_preimage
        let presig = payload.presig();
        assert_eq!(&tx.payload[..presig.len()], &presig[..]);

        let rest = transaction_v1_rest_preimage(&tx);
        let expected_digest = compute_sig_message(&rest, &presig);
        let sig_bytes = &tx.payload[presig.len()..presig.len() + 64];

        let msg = Message::from_digest(expected_digest);
        let xonly = secp256k1::XOnlyPublicKey::from_slice(&signer.pubkey()).unwrap();
        let sig = secp256k1::schnorr::Signature::from_slice(sig_bytes).unwrap();
        assert!(SECP256K1.verify_schnorr(&sig, &msg, &xonly).is_ok());
    }

    #[test]
    fn test_is_transient_submit_error() {
        assert!(is_transient_submit_error(&"transaction is an orphan"));
        assert!(is_transient_submit_error(&"request timed out"));
        assert!(is_transient_submit_error(&"RPC timeout"));
        assert!(!is_transient_submit_error(&"transaction rejected: invalid signature"));
        assert!(!is_transient_submit_error(&"fee too low"));
    }
}
