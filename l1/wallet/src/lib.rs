//! Keypair-backed L1 transaction building over any [`RpcApi`] client.
//!
//! These helpers build, sign, fund, and submit the transactions the L2 flow needs (covenant
//! bootstrap, lane activity, settlement funding). They depend only on `kaspa-consensus-core`
//! (signing, mass) and RPC, with no consensus internals, so they work against any node, in-process
//! or remote.

use std::cmp;

use kaspa_addresses::{Address, Prefix, Version};
use kaspa_consensus_core::{
    config::params::Params,
    constants::{TX_VERSION, TX_VERSION_TOCCATA},
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
use kaspa_rpc_core::{RpcError, RpcTransaction, api::rpc::RpcApi};
use kaspa_txscript::{pay_to_address_script, standard::pay_to_script_hash_script};
use secp256k1::Keypair;
use tap::Tap;
use vprogs_core_codec::Writer;
use vprogs_core_types::AccessMetadata;

/// A keypair-backed transaction issuer bound to an L1 node.
pub struct Wallet<'a, C: RpcApi + ?Sized> {
    /// RPC client used to fetch UTXOs and submit transactions.
    client: &'a C,
    /// Consensus parameters, used for mass calculation.
    params: &'a Params,
    /// Key that signs and funds every issued transaction.
    keypair: Keypair,
    /// P2PK address derived from `keypair` under the configured network prefix.
    address: Address,
}

impl<'a, C: RpcApi + ?Sized> Wallet<'a, C> {
    /// Builds a wallet whose address is the `prefix` P2PK address of `keypair`.
    pub fn new(client: &'a C, params: &'a Params, keypair: Keypair, prefix: Prefix) -> Self {
        let (xonly, _parity) = keypair.x_only_public_key();
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

        let utxos = self.fetch_spendable_utxos().await;
        assert!(
            utxos.len() >= payloads.len(),
            "not enough spendable UTXOs: found {} but need {}",
            utxos.len(),
            payloads.len(),
        );

        let script_public_key = pay_to_address_script(&self.address);

        payloads
            .into_iter()
            .zip(utxos)
            .map(|(payload, (outpoint, entry))| {
                // Kaspa-mempool-friendly heuristic: 10 × (input + output + base + payload).
                const FEE_PER_BYTE: u64 = 10;
                const INPUT_SIZE: u64 = 200;
                const OUTPUT_SIZE: u64 = 34;
                const BASE_OVERHEAD: u64 = 1000;
                let fee = FEE_PER_BYTE
                    * (INPUT_SIZE + OUTPUT_SIZE + BASE_OVERHEAD + payload.len() as u64);
                assert!(
                    entry.amount > fee,
                    "UTXO amount {} too small for fee {}",
                    entry.amount,
                    fee
                );

                let input = TransactionInput::new(outpoint, vec![], 0, 1);
                let output = TransactionOutput::new(entry.amount - fee, script_public_key.clone());

                sign(
                    MutableTransaction::with_entries(
                        Transaction::new(
                            tx_version,
                            vec![input],
                            vec![output],
                            0,
                            subnetwork_id,
                            0,
                            payload,
                        ),
                        vec![entry],
                    ),
                    self.keypair,
                )
                .tx
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
        let utxos = self.fetch_spendable_utxos().await;
        let (outpoint, entry) = utxos.into_iter().next().expect("no spendable UTXO for bootstrap");

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
        let signed =
            sign(MutableTransaction::with_entries(unsigned, entries.clone()), self.keypair).tx;
        self.commit_storage_mass(&signed, &entries);

        (signed, covenant_id)
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
        const FEE: u64 = 100_000;

        // Snapshot the covenant witness before signing - `sign` overwrites all signature scripts.
        let covenant_sig_script = settlement_tx.inputs[0].signature_script.clone();

        let utxos = self.fetch_spendable_utxos().await;
        let (fee_outpoint, fee_entry) =
            utxos.into_iter().next().expect("no spendable UTXO for fee");
        assert!(fee_entry.amount > FEE, "fee UTXO amount {} ≤ fee {FEE}", fee_entry.amount);

        let mut tx = settlement_tx;
        tx.inputs.push(TransactionInput::new(fee_outpoint, vec![], 0, 1));
        tx.outputs.push(TransactionOutput::new(
            fee_entry.amount - FEE,
            pay_to_address_script(&self.address),
        ));

        let entries = vec![covenant_entry, fee_entry];
        let signed = sign(MutableTransaction::with_entries(tx, entries.clone()), self.keypair);
        let mut tx = signed.tx;

        // Restore the covenant witness and set the covenant input's compute budget.
        tx.inputs[0].signature_script = covenant_sig_script;
        tx.inputs[0].mass = covenant_compute_budget.into();

        // Toccata txs must commit storage mass; compute it now that inputs and outputs are final.
        self.commit_storage_mass(&tx, &entries);

        // The signature script on input 0 changed, so recompute the on-the-wire id.
        tx.finalize();
        tx
    }

    /// Submits `tx` through the node's mempool, returning its id (or the RPC error).
    pub async fn submit_transaction(&self, tx: &Transaction) -> Result<Hash, RpcError> {
        self.client.submit_transaction(RpcTransaction::from(tx), false).await?;
        Ok(tx.id())
    }

    /// Number of currently spendable UTXOs for the issuer address.
    pub async fn spendable_utxo_count(&self) -> usize {
        self.fetch_spendable_utxos().await.len()
    }

    /// Commits KIP-0009 storage mass on `tx` (Toccata txs must carry it for the node to accept
    /// them).
    fn commit_storage_mass(&self, tx: &Transaction, entries: &[UtxoEntry]) {
        let calc = MassCalculator::new(
            self.params.mass_per_tx_byte,
            self.params.mass_per_script_pub_key_byte,
            self.params.storage_mass_parameter,
        );
        let populated = PopulatedTransaction::new(tx, entries.to_vec());
        let masses = calc
            .calc_contextual_masses(&populated)
            .expect("contextual mass calculation must succeed for a populated transaction");
        tx.set_mass(masses.storage_mass);
    }

    /// Spendable UTXOs for the issuer address (matured coinbases only), largest first. Requires the
    /// node to run with `--utxoindex`.
    async fn fetch_spendable_utxos(&self) -> Vec<(TransactionOutpoint, UtxoEntry)> {
        let virtual_daa_score = self.client.get_server_info().await.unwrap().virtual_daa_score;

        self.client
            .get_utxos_by_addresses(vec![self.address.clone()])
            .await
            .unwrap()
            .into_iter()
            .filter(|e| {
                !e.utxo_entry.is_coinbase
                    || e.utxo_entry.block_daa_score + self.params.blockrate.coinbase_maturity
                        <= virtual_daa_score
            })
            .map(|e| (TransactionOutpoint::from(e.outpoint), UtxoEntry::from(e.utxo_entry)))
            .collect::<Vec<_>>()
            .tap_mut(|utxos| utxos.sort_by_key(|b| cmp::Reverse(b.1.amount)))
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
