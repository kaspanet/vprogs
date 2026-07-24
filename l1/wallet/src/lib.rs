//! Keypair-backed L1 transaction building over any [`RpcApi`] client.
//!
//! These helpers build, sign, fund, and submit the transactions the L2 flow needs (covenant
//! bootstrap, lane activity, settlement funding). They depend only on `kaspa-consensus-core`
//! (signing, mass) and RPC, with no consensus internals, so they work against any node, in-process
//! or remote.
//!
//! The actual tx construction (sign, fee, storage mass, covenant binding, witness restore) lives in
//! [`build`] as pure functions over an already-chosen `(outpoint, entry)`. [`Wallet`] is the thin
//! RPC layer that fetches spendable UTXOs and submits; a caller that already holds its spendable
//! set (e.g. an in-process simulation) reuses [`build`] directly without any RPC.
//!
//! TODO: this is a POC wallet. UTXO selection avoids re-spending unconfirmed outputs with a
//! caller-threaded in-flight set ([`Wallet::build_activity_excluding`]); a production issuer should
//! track pending transactions directly, or reuse the kaspa wallet framework.

use std::cmp;

use kaspa_addresses::{Address, Prefix, Version};
use kaspa_consensus_core::{
    config::params::Params,
    constants::{TX_VERSION, TX_VERSION_TOCCATA},
    mass::units::ComputeBudget,
    subnets::{SUBNETWORK_ID_NATIVE, SubnetworkId},
    tx::{Transaction, TransactionOutpoint, TransactionOutput, UtxoEntry},
};
use kaspa_hashes::Hash;
use kaspa_rpc_core::{RpcError, RpcTransaction, api::rpc::RpcApi};
use secp256k1::Keypair;
use tap::Tap;
use vprogs_core_codec::Writer;
use vprogs_core_types::AccessMetadata;

pub mod build;

/// A keypair-backed transaction issuer bound to an L1 node.
pub struct Wallet<'a, C: RpcApi + ?Sized> {
    /// RPC client used to fetch UTXOs and submit transactions.
    client: &'a C,
    /// Consensus parameters, used for mass calculation.
    params: &'a Params,
    /// Key that signs and funds every issued transaction.
    keypair: Keypair,
    /// P2PK address derived from `keypair`, prefixed for the network in `params`.
    address: Address,
}

impl<'a, C: RpcApi + ?Sized> Wallet<'a, C> {
    /// Builds a wallet whose address is the `keypair`'s P2PK address under the network prefix
    /// implied by `params` (so the prefix can never disagree with the consensus params).
    pub fn new(client: &'a C, params: &'a Params, keypair: Keypair) -> Self {
        let (xonly, _parity) = keypair.x_only_public_key();
        let prefix = Prefix::from(params.net.network_type());
        let address = Address::new(prefix, Version::PubKey, &xonly.serialize());
        Self { client, params, keypair, address }
    }

    /// The address activity / bootstrap / fees are funded from.
    pub fn address(&self) -> &Address {
        &self.address
    }

    /// Builds signed native-subnetwork transactions, each carrying one payload.
    pub async fn build_payload_transactions(&self, payloads: Vec<Vec<u8>>) -> Vec<Transaction> {
        self.build_subnet_payload_transactions(payloads, SUBNETWORK_ID_NATIVE, TX_VERSION).await
    }

    /// Builds one signed transaction per payload, each on `subnetwork_id` with `tx_version`. For
    /// lane activity pass the lane subnetwork and [`TX_VERSION_TOCCATA`].
    pub async fn build_subnet_payload_transactions(
        &self,
        payloads: Vec<Vec<u8>>,
        subnetwork_id: SubnetworkId,
        tx_version: u16,
    ) -> Vec<Transaction> {
        // User-lane subnetwork shape: 4-byte namespace followed by 16 zero bytes.
        debug_assert!(
            subnetwork_id == SUBNETWORK_ID_NATIVE
                || subnetwork_id.as_bytes()[4..].iter().all(|&b| b == 0),
            "non-native subnetwork id must have 16 zero bytes after the 4-byte namespace",
        );
        // Non-native subnetworks require post-covenant tx version.
        debug_assert!(
            subnetwork_id == SUBNETWORK_ID_NATIVE || tx_version >= TX_VERSION_TOCCATA,
            "non-native subnetwork requires tx_version >= TX_VERSION_TOCCATA",
        );

        let utxos = self.fetch_spendable_utxos().await.expect("fetch spendable utxos");
        assert!(
            utxos.len() >= payloads.len(),
            "not enough spendable UTXOs: found {} but need {}",
            utxos.len(),
            payloads.len(),
        );

        payloads
            .into_iter()
            .zip(utxos)
            .map(|(payload, (outpoint, entry))| {
                build::activity_transaction(build::ActivityTx {
                    payload,
                    outpoint,
                    entry,
                    keypair: self.keypair,
                    address: &self.address,
                    subnetwork_id,
                    tx_version,
                    params: self.params,
                })
            })
            .collect()
    }

    /// Builds a signed carrier transaction on `subnetwork_id` whose L2 payload is produced by
    /// `finalize_payload` over the carrier's `rest_preimage`, funded from the wallet's largest
    /// spendable UTXO. `extra_outputs` are prepended before the change (e.g. a deposit funding
    /// output). Use this for runtime carriers whose payload signature commits to the post-funding
    /// outputs. The returned tx is ready to `mine_block` or `submit_transaction`.
    pub async fn build_signed_carrier<F: Fn(&[u8]) -> Vec<u8>>(
        &self,
        extra_outputs: Vec<TransactionOutput>,
        subnetwork_id: SubnetworkId,
        tx_version: u16,
        finalize_payload: F,
    ) -> Transaction {
        let utxos = self.fetch_spendable_utxos().await.expect("fetch spendable utxos");
        let (outpoint, entry) = utxos.into_iter().next().expect("no spendable UTXO for carrier");
        build::signed_carrier_transaction(build::SignedCarrierTx {
            outpoint,
            entry,
            keypair: self.keypair,
            change_address: &self.address,
            subnetwork_id,
            tx_version,
            params: self.params,
            extra_outputs,
            finalize_payload,
        })
    }

    /// Builds a signed bootstrap transaction whose single output is P2SH(`redeem_script`) with a
    /// genesis covenant binding. Returns the tx and the covenant id consensus will recompute.
    pub async fn build_covenant_bootstrap_transaction(
        &self,
        redeem_script: &[u8],
        value: u64,
    ) -> (Transaction, Hash) {
        let utxos = self.fetch_spendable_utxos().await.expect("fetch spendable utxos");
        let (outpoint, entry) = utxos.into_iter().next().expect("no spendable UTXO for bootstrap");
        build::covenant_bootstrap_transaction(
            redeem_script,
            value,
            outpoint,
            entry,
            self.keypair,
            self.params,
        )
    }

    /// Funds and signs a settlement transaction without submitting it: appends a fee input + change
    /// output, signs only the fee input, preserves the covenant input's witness, sets the covenant
    /// input's compute budget, and commits the storage-mass field. The result is ready to submit.
    pub async fn prepare_settlement_transaction(
        &self,
        settlement_tx: Transaction,
        covenant_entry: UtxoEntry,
        covenant_compute_budget: ComputeBudget,
    ) -> Transaction {
        self.prepare_settlement_excluding(
            settlement_tx,
            covenant_entry,
            covenant_compute_budget,
            &std::collections::HashSet::new(),
        )
        .await
        .expect("fetch spendable utxos")
        .expect("no spendable UTXO for fee")
        .0
    }

    /// Like [`Wallet::prepare_settlement_transaction`], but funds the fee from the largest
    /// spendable UTXO **not** in `excluded`, and also returns the fee outpoint it spent. Returns
    /// `Ok(None)` when every spendable UTXO is excluded, or the RPC error when the spendable set
    /// cannot be fetched (a transient node timeout the caller retries, rather than a hard failure).
    ///
    /// The node can reject a settlement as an orphan when its fee input references an output it has
    /// not yet accepted into its DAG. The caller re-prepares with that outpoint added to `excluded`
    /// so the retry funds from a different, settled UTXO.
    pub async fn prepare_settlement_excluding(
        &self,
        settlement_tx: Transaction,
        covenant_entry: UtxoEntry,
        covenant_compute_budget: ComputeBudget,
        excluded: &std::collections::HashSet<TransactionOutpoint>,
    ) -> Result<Option<(Transaction, TransactionOutpoint)>, RpcError> {
        let utxos = self.fetch_spendable_utxos().await?;
        let Some((fee_outpoint, fee_entry)) =
            utxos.into_iter().find(|(o, _)| !excluded.contains(o))
        else {
            return Ok(None);
        };
        let tx = build::settlement_transaction(build::SettlementTx {
            settlement_tx,
            covenant_entry,
            covenant_compute_budget,
            fee_outpoint,
            fee_entry,
            keypair: self.keypair,
            address: &self.address,
            params: self.params,
        });
        Ok(Some((tx, fee_outpoint)))
    }

    /// Builds and signs (without submitting) a transaction paying `count` outputs of `value` sompi
    /// each to `recipient`, funded from the largest spendable UTXO, with the remainder returned as
    /// change to this wallet's own address. Used to seed a distinct funding address (e.g. another
    /// prover's fee key) from this wallet's coinbase.
    pub async fn pay_to_address(
        &self,
        recipient: &Address,
        value: u64,
        count: usize,
    ) -> Transaction {
        let utxos = self.fetch_spendable_utxos().await.expect("fetch spendable utxos");
        let (outpoint, entry) =
            utxos.into_iter().next().expect("no spendable UTXO for pay_to_address");
        build::pay_to_address_transaction(build::PayToAddressTx {
            outpoint,
            entry,
            recipient,
            value,
            count,
            keypair: self.keypair,
            change_address: &self.address,
            params: self.params,
        })
    }

    /// Submits `tx` through the node's mempool, returning its id (or the RPC error).
    pub async fn submit_transaction(&self, tx: &Transaction) -> Result<Hash, RpcError> {
        self.client.submit_transaction(RpcTransaction::from(tx), false).await?;
        Ok(tx.id())
    }

    /// Builds one signed `subnetwork_id` activity tx spending the largest spendable UTXO **not** in
    /// `in_flight`, returning the tx and the outpoint it spends. Returns `Ok(None)` when every
    /// spendable UTXO is in flight, and the RPC error if the spendable-set fetch fails (so a
    /// long-running issuer can log and retry instead of crashing).
    ///
    /// `get_utxos_by_addresses` reports the confirmed UTXO set, which still includes a UTXO already
    /// spent by an unconfirmed mempool transaction (its change output stays invisible until mined).
    /// A tight issuer loop therefore keeps re-selecting the same largest UTXO and the node rejects
    /// the double spend. The caller threads the set of outpoints it has spent this session through
    /// `in_flight`: this method first prunes that set to the outpoints the node still reports (so a
    /// UTXO that has since been mined away, and replaced by a fresh change outpoint, becomes
    /// selectable again), then skips the rest to pick the next-largest free UTXO. The caller
    /// inserts the returned outpoint on a successful (or double-spend-rejected) submit.
    pub async fn build_activity_excluding(
        &self,
        payload: Vec<u8>,
        subnetwork_id: SubnetworkId,
        tx_version: u16,
        in_flight: &mut std::collections::HashSet<TransactionOutpoint>,
    ) -> Result<Option<(Transaction, TransactionOutpoint)>, RpcError> {
        let utxos = self.fetch_spendable_utxos().await?;
        let present: std::collections::HashSet<TransactionOutpoint> =
            utxos.iter().map(|(o, _)| *o).collect();
        in_flight.retain(|o| present.contains(o));
        let Some((outpoint, entry)) = utxos.into_iter().find(|(o, _)| !in_flight.contains(o))
        else {
            return Ok(None);
        };
        let tx = build::activity_transaction(build::ActivityTx {
            payload,
            outpoint,
            entry,
            keypair: self.keypair,
            address: &self.address,
            subnetwork_id,
            tx_version,
            params: self.params,
        });
        Ok(Some((tx, outpoint)))
    }

    /// Spendable UTXOs for the issuer address (matured coinbases only), largest first. Requires the
    /// node to run with `--utxoindex`.
    ///
    /// Propagates the RPC error rather than unwrapping: a long-running issuer must survive a
    /// transient node hiccup (log it and retry next tick), not crash the process. One-shot
    /// startup callers (bootstrap) `.expect()` it, where a failure should abort.
    pub async fn fetch_spendable_utxos(
        &self,
    ) -> Result<Vec<(TransactionOutpoint, UtxoEntry)>, RpcError> {
        let virtual_daa_score = self.client.get_server_info().await?.virtual_daa_score;

        Ok(self
            .client
            .get_utxos_by_addresses(vec![self.address.clone()])
            .await?
            .into_iter()
            .filter(|e| {
                !e.utxo_entry.is_coinbase
                    || e.utxo_entry.block_daa_score + self.params.blockrate.coinbase_maturity
                        <= virtual_daa_score
            })
            .map(|e| (TransactionOutpoint::from(e.outpoint), UtxoEntry::from(e.utxo_entry)))
            .collect::<Vec<_>>()
            .tap_mut(|utxos| utxos.sort_by_key(|b| cmp::Reverse(b.1.amount))))
    }
}

/// Encodes an activity payload: a length-prefixed [`AccessMetadata`] list (strictly ascending by
/// resource id) followed by the instruction bytes. Mirrors the scheduler's decode path
/// (`AccessMetadata::decode_vec` then the remaining instruction bytes).
pub fn encode_activity_payload(meta: &[AccessMetadata], ix: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.write_slice(meta);
    buf.write(ix);
    buf
}
