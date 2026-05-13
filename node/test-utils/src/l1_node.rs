//! A single Kaspa L1 node for testing.

use std::{cmp, io::Write, time::Duration};

use kaspa_addresses::Address;
use kaspa_consensus_core::{
    Hash,
    config::params::{OverrideParams, Params, SIMNET_PARAMS},
    constants::{TX_VERSION, TX_VERSION_TOCCATA},
    hashing::covenant_id::covenant_id,
    header::Header,
    mass::{MassCalculator, units::ComputeBudget},
    merkle::calc_hash_merkle_root,
    sign::sign,
    subnets::{SUBNETWORK_ID_NATIVE, SubnetworkId},
    tx::{
        CovenantBinding, MutableTransaction, Transaction, TransactionInput, TransactionOutpoint,
        TransactionOutput, UtxoEntry,
    },
};
use kaspa_grpc_client::GrpcClient;
use kaspa_rpc_core::{RpcTransaction, api::rpc::RpcApi};
use kaspa_testing_integration::common::daemon::Daemon;
use kaspa_txscript::{pay_to_address_script, standard::pay_to_script_hash_script};
use kaspa_wrpc_server::address::WrpcNetAddress;
use kaspad_lib::args::Args;
use secp256k1::Keypair;
use tap::Tap;

/// An in-process Kaspa simnet node for integration tests.
///
/// Wraps a [`Daemon`] and its gRPC client, providing helpers for mining blocks and connecting
/// peers. The daemon shuts down automatically on drop.
pub struct L1Node {
    /// Kaspa daemon process.
    daemon: Daemon,
    /// gRPC client for RPC calls (mining, peer management).
    grpc_client: GrpcClient,
    /// Coinbase address used for mining rewards.
    address: Address,
    /// Keypair used for signing transactions.
    keypair: Keypair,
    /// Consensus parameters applied to this node.
    params: Params,
}

impl L1Node {
    /// Creates and starts a new isolated simnet node.
    ///
    /// Pass a customization closure to override consensus parameters. The closure receives the
    /// default simnet [`Params`] to mutate. Pass `None` for vanilla simnet defaults.
    ///
    /// ```no_run
    /// # use vprogs_node_test_utils::L1Node;
    /// # async fn example() {
    /// // Vanilla simnet defaults:
    /// let node = L1Node::new(None).await;
    ///
    /// // Fast coinbase maturity for UTXO tests:
    /// let node = L1Node::new(Some(|p| p.blockrate.coinbase_maturity = 10)).await;
    /// # }
    /// ```
    pub async fn new(customize: Option<fn(&mut Params)>) -> Self {
        kaspa_core::log::try_init_logger("INFO");

        // Generate a real keypair for signing transactions.
        let keypair = Keypair::new(secp256k1::SECP256K1, &mut secp256k1::rand::thread_rng());
        let (xonly, _parity) = keypair.x_only_public_key();
        let address = Address::new(
            kaspa_addresses::Prefix::Simnet,
            kaspa_addresses::Version::PubKey,
            &xonly.serialize(),
        );

        // Start from the default simnet params and let the caller customize them.
        let mut params = SIMNET_PARAMS;
        if let Some(f) = customize {
            f(&mut params);
        }

        // Serialize the params to a temp file the daemon reads on startup. The file's lifetime is
        // tied to this scope - if the daemon ever re-reads `override_params_file` post-startup,
        // it'll find nothing.
        let overrides = OverrideParams::from(params.clone());
        let mut params_file =
            tempfile::NamedTempFile::new().expect("failed to create override-params tempfile");
        write!(
            params_file,
            "{}",
            serde_json::to_string(&overrides).expect("OverrideParams must serialize"),
        )
        .expect("failed to write override-params tempfile");

        // Spawn a simnet daemon with unsafe RPC, unsynced mining, and UTXO index enabled
        // so we can mine blocks without waiting for IBD and query UTXOs by address.
        let mut daemon = Daemon::new_random_with_args(
            Args {
                simnet: true,
                unsafe_rpc: true,
                enable_unsynced_mining: true,
                disable_upnp: true,
                utxoindex: true,
                override_params_file: Some(
                    params_file.path().to_str().expect("tempfile path must be utf-8").to_string(),
                ),
                ..Default::default()
            },
            10, // fd budget
        );
        let grpc_client = daemon.start().await;

        Self { daemon, grpc_client, address, keypair, params }
    }

    /// Returns a reference to the consensus parameters.
    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Returns a reference to the underlying daemon.
    pub fn daemon(&self) -> &Daemon {
        &self.daemon
    }

    /// Returns a reference to the gRPC client for direct RPC calls (block lookup, UTXO queries).
    pub fn grpc_client(&self) -> &GrpcClient {
        &self.grpc_client
    }

    /// Returns the wRPC Borsh URL for connecting to this node.
    pub fn wrpc_borsh_url(&self) -> String {
        // Extract the port from the daemon's Borsh listen address.
        let args = self.daemon.args.read();
        let port = match args.rpclisten_borsh.as_ref().unwrap() {
            WrpcNetAddress::Custom(addr) => addr.normalize(0).port,
            _ => panic!("expected Custom address with port"),
        };
        format!("ws://127.0.0.1:{}", port)
    }

    /// Connects this node to another node as a peer. Polls until the peer appears in the connected
    /// peer list.
    pub async fn connect_to(&self, other: &L1Node) {
        let other_port = other.daemon().p2p_port;

        self.grpc_client
            .add_peer(format!("127.0.0.1:{}", other_port).try_into().unwrap(), true)
            .await
            .unwrap();

        // Poll until the peer shows up in the connected peer list.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let peers = self.grpc_client.get_connected_peer_info().await.unwrap();
            if peers.peer_info.iter().any(|p| p.address.port == other_port) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for peer connection"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Mines a single block, injecting any pre-built L1 transactions into the block template.
    ///
    /// Note: in Kaspa DAG consensus, a block's transactions are accepted by the next chain block.
    /// The caller must mine an additional block for the transactions to be accepted.
    pub async fn mine_block(&self, txs: &[Transaction]) -> Hash {
        let mut template =
            self.grpc_client.get_block_template(self.address.clone(), vec![]).await.unwrap();

        if !txs.is_empty() {
            for tx in txs {
                template.block.transactions.push(RpcTransaction::from(tx));
            }

            // Recompute the hash merkle root to cover the added transactions.
            let consensus_txs: Vec<Transaction> = template
                .block
                .transactions
                .iter()
                .map(|rpc_tx| Transaction::try_from(rpc_tx.clone()).unwrap())
                .collect();
            template.block.header.hash_merkle_root = calc_hash_merkle_root(consensus_txs.iter());
        }

        let header: Header = (&template.block.header).try_into().unwrap();
        let hash = header.hash;
        self.grpc_client.submit_block(template.block, false).await.unwrap();
        hash
    }

    /// Mines `count` empty blocks sequentially and returns their hashes.
    pub async fn mine_blocks(&self, count: usize) -> Vec<Hash> {
        let mut hashes = Vec::with_capacity(count);
        for _ in 0..count {
            hashes.push(self.mine_block(&[]).await);
        }
        hashes
    }

    /// Mines enough blocks so that `num_utxos` coinbase UTXOs become spendable.
    ///
    /// Each block produces one coinbase UTXO that matures after `coinbase_maturity` blocks. The
    /// genesis child starts at `daa_score = 2`, so we add a small offset to ensure the requested
    /// UTXOs are fully mature.
    pub async fn mine_utxos(&self, num_utxos: usize) -> Vec<Hash> {
        // +2 accounts for the genesis daa_score offset.
        self.mine_blocks(self.params.blockrate.coinbase_maturity as usize + num_utxos + 2).await
    }

    /// Disconnects the gRPC client. The daemon shuts down on drop.
    pub async fn shutdown(self) {
        self.grpc_client.disconnect().await.unwrap()
    }

    /// Builds signed L1 transactions, each carrying the given payload.
    ///
    /// Requires enough spendable UTXOs (call [`mine_utxos`](Self::mine_utxos) first).
    pub async fn build_payload_transactions(&self, payloads: Vec<Vec<u8>>) -> Vec<Transaction> {
        self.build_subnet_payload_transactions(payloads, SUBNETWORK_ID_NATIVE, TX_VERSION).await
    }

    /// Builds signed transactions tagged with the given `subnetwork_id` and `tx_version`. For
    /// native-subnetwork txs prefer [`Self::build_payload_transactions`].
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
                // Approx tx mass × per-byte multiplier. Kaspa-mempool-friendly heuristic:
                //   10 × (input_size(~200) + output_size(~34) + base_overhead(1000) + payload).
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

    /// Funds and signs a settlement transaction without submitting it: appends a fee input +
    /// change output, signs only the fee input, preserves the covenant input's witness, sets
    /// the covenant input's compute budget, and commits the storage-mass field. The returned
    /// transaction is ready to ship - either via [`Self::mine_block`] (deterministic chain
    /// inclusion) or [`Self::submit_settlement_transaction`] (routed through the mempool).
    ///
    /// `covenant_compute_budget` sets the compute budget on input 0 (the covenant). The R0Succinct
    /// precompile alone consumes ~2500 budget units; production callers pass ~10000 to leave room
    /// for surrounding script ops. Dev-mode covenants without the precompile can pass ~100.
    pub async fn prepare_settlement_transaction(
        &self,
        settlement_tx: Transaction,
        covenant_entry: UtxoEntry,
        covenant_compute_budget: ComputeBudget,
    ) -> Transaction {
        // Approximate fee/mass; bump if mempool rejects.
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

        // Toccata transactions must commit their storage mass; compute it now that all inputs and
        // outputs are finalized.
        self.commit_storage_mass(&tx, &entries);

        // `sign` left the wrong tx id in place because the signature script changed on input 0;
        // recompute so callers see the on-the-wire id.
        tx.finalize();
        tx
    }

    /// Submits a settlement transaction through the mempool via gRPC.
    ///
    /// Note: in Kaspa DAG consensus, a block's transactions are accepted by the next chain block
    /// (caller must mine an additional block for acceptance). This routes through mempool
    /// standardness checks; for deterministic chain inclusion that bypasses mempool timing,
    /// prefer building the tx with [`Self::prepare_settlement_transaction`] and shipping it
    /// directly via [`Self::mine_block`].
    pub async fn submit_settlement_transaction(
        &self,
        settlement_tx: Transaction,
        covenant_entry: UtxoEntry,
        covenant_compute_budget: ComputeBudget,
    ) -> Hash {
        let tx = self
            .prepare_settlement_transaction(settlement_tx, covenant_entry, covenant_compute_budget)
            .await;
        let tx_id = tx.id();
        self.grpc_client
            .submit_transaction(RpcTransaction::from(&tx), false)
            .await
            .expect("settlement tx submission failed");
        tx_id
    }

    /// Builds a signed transaction whose single output is pay-to-script-hash of `redeem_script`,
    /// annotated with a genesis covenant binding.
    ///
    /// Returns the signed transaction and the covenant id that the consensus validator will
    /// recompute from the input outpoint + output.
    pub async fn build_covenant_bootstrap_transaction(
        &self,
        redeem_script: &[u8],
        value: u64,
    ) -> (Transaction, Hash) {
        let utxos = self.fetch_spendable_utxos().await;
        let (outpoint, entry) = utxos.into_iter().next().expect("no spendable UTXO");

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

        // Bootstrap is a Toccata-version tx; the storage-mass commitment must match what the
        // block validator recomputes from the populated transaction.
        self.commit_storage_mass(&signed, &entries);

        (signed, covenant_id)
    }

    /// Computes the storage mass (KIP-0009) for `tx` populated with `entries` and stores it
    /// on the transaction's mass commitment field. Toccata-version transactions must commit
    /// the correct storage mass for the block validator to accept them.
    fn commit_storage_mass(&self, tx: &Transaction, entries: &[UtxoEntry]) {
        let calc = MassCalculator::new(
            self.params.mass_per_tx_byte,
            self.params.mass_per_script_pub_key_byte,
            self.params.storage_mass_parameter,
        );
        let populated = kaspa_consensus_core::tx::PopulatedTransaction::new(tx, entries.to_vec());
        let masses = calc
            .calc_contextual_masses(&populated)
            .expect("contextual mass calculation must succeed for a populated transaction");
        tx.set_mass(masses.storage_mass);
    }

    /// Fetches spendable UTXOs for the miner address, sorted by amount (largest first).
    async fn fetch_spendable_utxos(&self) -> Vec<(TransactionOutpoint, UtxoEntry)> {
        let virtual_daa_score = self.grpc_client.get_server_info().await.unwrap().virtual_daa_score;

        self.grpc_client
            .get_utxos_by_addresses(vec![self.address.clone()])
            .await
            .unwrap()
            .into_iter()
            .filter(|e| {
                // Coinbase UTXOs require `coinbase_maturity` confirmations before spending
                !e.utxo_entry.is_coinbase
                    || e.utxo_entry.block_daa_score + self.params.blockrate.coinbase_maturity
                        <= virtual_daa_score
            })
            .map(|e| (TransactionOutpoint::from(e.outpoint), UtxoEntry::from(e.utxo_entry)))
            .collect::<Vec<_>>()
            .tap_mut(|utxos| utxos.sort_by_key(|b| cmp::Reverse(b.1.amount)))
    }
}
