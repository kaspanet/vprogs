//! Keypair-backed L1 transaction building over any [`RpcApi`] client.
//!
//! These helpers build, sign, fund, and submit the transactions the L2 flow needs (covenant
//! bootstrap, lane activity, settlement funding). They depend only on `kaspa-consensus-core`
//! (signing, mass) and RPC, with no consensus internals, so they work against any node, in-process
//! or remote.
//!
//! The actual tx construction (sign, fee, storage mass, covenant binding, witness restore) lives in
//! [`build`] as pure functions over a preference-ordered candidate list of spendable `(outpoint,
//! entry)` pairs. [`Wallet`] is the thin RPC layer that fetches spendable UTXOs and submits; a
//! caller that already holds its spendable set (e.g. an in-process simulation) reuses [`build`]
//! directly without any RPC.
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
    tx::{Transaction, TransactionOutpoint, UtxoEntry},
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

    /// The fee policy for a new build: [`build::FeePolicy::TargetFeerate`] at the node's
    /// sub-minute (`normal_buckets[0]`) rate, falling back to the priority bucket if the vector
    /// is unexpectedly empty, or to [`build::FeePolicy::Floor`] when the estimate RPC fails
    /// (estimator unavailability must not stall issuance).
    async fn fee_policy(&self) -> build::FeePolicy {
        match self.client.get_fee_estimate().await {
            Ok(estimate) => {
                let feerate = estimate
                    .normal_buckets
                    .first()
                    .map_or(estimate.priority_bucket.feerate, |bucket| bucket.feerate);
                build::FeePolicy::TargetFeerate(feerate)
            }
            Err(e) => {
                log::warn!("fee estimate fetch failed, falling back to the floor policy: {e}");
                build::FeePolicy::Floor
            }
        }
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

        let mut utxos = self.fetch_spendable_utxos().await.expect("fetch spendable utxos");
        payloads
            .into_iter()
            .map(|payload| {
                let tx = build::activity_transaction(build::ActivityTx {
                    payload,
                    candidates: utxos.clone(),
                    keypair: self.keypair,
                    address: &self.address,
                    subnetwork_id,
                    tx_version,
                    fee_policy: build::FeePolicy::Floor,
                    params: self.params,
                })
                .expect("spendable UTXOs must fund the activity fee");
                let spent: std::collections::HashSet<TransactionOutpoint> =
                    tx.inputs.iter().map(|input| input.previous_outpoint).collect();
                utxos.retain(|(outpoint, _)| !spent.contains(outpoint));
                tx
            })
            .collect()
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

    /// Funds and signs a settlement transaction without submitting it: appends fee inputs from a
    /// prefix of the spendable UTXOs plus a change output, signs the fee inputs, preserves the
    /// covenant input's witness, sets the covenant input's compute budget, and commits the
    /// storage-mass field. The result is ready to submit.
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
        .expect("no spendable UTXO can fund the settlement fee")
        .0
    }

    /// Like [`Wallet::prepare_settlement_transaction`], but funds the fee from spendable UTXOs
    /// **not** in `excluded`, and also returns the fee outpoints it spent. Returns `None` when
    /// every spendable UTXO is excluded, or the free ones cannot fund the fee.
    ///
    /// The node can reject a settlement as an orphan when its fee input references an output it has
    /// not yet accepted into its DAG. The caller re-prepares with that outpoint added to `excluded`
    /// so the retry funds from different, settled UTXOs.
    pub async fn prepare_settlement_excluding(
        &self,
        settlement_tx: Transaction,
        covenant_entry: UtxoEntry,
        covenant_compute_budget: ComputeBudget,
        excluded: &std::collections::HashSet<TransactionOutpoint>,
    ) -> Option<(Transaction, Vec<TransactionOutpoint>)> {
        let utxos = self.fetch_spendable_utxos().await.expect("fetch spendable utxos");
        let fee_candidates: Vec<_> =
            utxos.into_iter().filter(|(outpoint, _)| !excluded.contains(outpoint)).collect();
        if fee_candidates.is_empty() {
            return None;
        }
        let fee_policy = self.fee_policy().await;
        let tx = build::settlement_transaction(build::SettlementTx {
            settlement_tx,
            covenant_entry,
            covenant_compute_budget,
            fee_candidates,
            keypair: self.keypair,
            address: &self.address,
            fee_policy,
            params: self.params,
        })
        .inspect_err(|e| log::warn!("settlement fee funding failed: {e}"))
        .ok()?;
        let fee_outpoints = tx.inputs[1..].iter().map(|input| input.previous_outpoint).collect();
        Some((tx, fee_outpoints))
    }

    /// Builds and signs (without submitting) a transaction paying `count` outputs of `value` sompi
    /// each to `recipient`, funded from a prefix of the spendable UTXOs, with the remainder
    /// returned as change to this wallet's own address when the remainder is storage-viable, and
    /// folded into the fee otherwise. Used to seed a distinct funding address (e.g. another
    /// prover's fee key) from this wallet's coinbase.
    pub async fn pay_to_address(
        &self,
        recipient: &Address,
        value: u64,
        count: usize,
    ) -> Transaction {
        let utxos = self.fetch_spendable_utxos().await.expect("fetch spendable utxos");
        build::pay_to_address_transaction(build::PayToAddressTx {
            candidates: utxos,
            recipient,
            value,
            count,
            keypair: self.keypair,
            change_address: &self.address,
            params: self.params,
        })
        .expect("spendable UTXOs must fund the payout")
    }

    /// Submits `tx` through the node's mempool, returning its id (or the RPC error).
    pub async fn submit_transaction(&self, tx: &Transaction) -> Result<Hash, RpcError> {
        self.client.submit_transaction(RpcTransaction::from(tx), false).await?;
        Ok(tx.id())
    }

    /// Builds one signed `subnetwork_id` activity tx funded from the spendable UTXOs **not** in
    /// `in_flight`. Returns `Ok(None)` when every spendable UTXO is in flight, or the free ones
    /// cannot cover the fee, and the RPC error if the spendable-set fetch fails (so a
    /// long-running issuer can log and retry instead of crashing).
    ///
    /// `get_utxos_by_addresses` reports the confirmed UTXO set, which still includes a UTXO already
    /// spent by an unconfirmed mempool transaction (its change output stays invisible until mined).
    /// A tight issuer loop therefore keeps re-selecting the same largest UTXO and the node rejects
    /// the double spend. The caller threads the set of outpoints it has spent this session through
    /// `in_flight`: this method first prunes that set to the outpoints the node still reports (so a
    /// UTXO that has since been mined away, and replaced by a fresh change outpoint, becomes
    /// selectable again), then funds from the rest. The caller extends `in_flight` with the
    /// returned tx's spent outpoints on a successful (or double-spend-rejected) submit.
    pub async fn build_activity_excluding(
        &self,
        payload: Vec<u8>,
        subnetwork_id: SubnetworkId,
        tx_version: u16,
        in_flight: &mut std::collections::HashSet<TransactionOutpoint>,
    ) -> Result<Option<Transaction>, RpcError> {
        let utxos = self.fetch_spendable_utxos().await?;
        let present: std::collections::HashSet<TransactionOutpoint> =
            utxos.iter().map(|(o, _)| *o).collect();
        in_flight.retain(|o| present.contains(o));
        let candidates: Vec<(TransactionOutpoint, UtxoEntry)> =
            utxos.into_iter().filter(|(o, _)| !in_flight.contains(o)).collect();
        if candidates.is_empty() {
            return Ok(None);
        }
        let fee_policy = self.fee_policy().await;
        Ok(build::activity_transaction(build::ActivityTx {
            payload,
            candidates,
            keypair: self.keypair,
            address: &self.address,
            subnetwork_id,
            tx_version,
            fee_policy,
            params: self.params,
        })
        .inspect_err(|e| log::warn!("activity funding failed: {e}"))
        .ok())
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
