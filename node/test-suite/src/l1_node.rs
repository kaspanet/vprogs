//! A single Kaspa L1 node for testing.

use std::{io::Write, time::Duration};

use borsh::BorshSerialize;
use kaspa_addresses::Address;
use kaspa_consensus_core::{
    Hash,
    config::params::{OverrideParams, Params, SIMNET_PARAMS},
    constants::TX_VERSION,
    header::Header,
    merkle::calc_hash_merkle_root,
    sign::sign,
    subnets::SUBNETWORK_ID_NATIVE,
    tx::{
        MutableTransaction, Transaction, TransactionInput, TransactionOutpoint, TransactionOutput,
        UtxoEntry,
    },
};
use kaspa_grpc_client::GrpcClient;
use kaspa_rpc_core::{RpcTransaction, api::rpc::RpcApi};
use kaspa_testing_integration::common::daemon::Daemon;
use kaspa_txscript::pay_to_address_script;
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
    /// # use vprogs_node_test_suite::L1Node;
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

        // Serialize the params to a temp file that the daemon reads on startup.
        let overrides = OverrideParams::from(params.clone());
        let mut params_file = tempfile::NamedTempFile::new().unwrap();
        write!(params_file, "{}", serde_json::to_string(&overrides).unwrap()).unwrap();

        // Spawn a simnet daemon with unsafe RPC, unsynced mining, and UTXO index enabled
        // so we can mine blocks without waiting for IBD and query UTXOs by address.
        let mut daemon = Daemon::new_random_with_args(
            Args {
                simnet: true,
                unsafe_rpc: true,
                enable_unsynced_mining: true,
                disable_upnp: true,
                utxoindex: true,
                override_params_file: Some(params_file.path().to_str().unwrap().to_string()),
                ..Default::default()
            },
            10,
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

    /// Mines a single block, optionally injecting L2 transactions into the block template.
    ///
    /// Each L2 transaction is borsh-serialized into the `payload` field of a separate L1
    /// transaction. Requires mature UTXOs when payloads are provided (call
    /// [`mine_utxos`](Self::mine_utxos) first).
    ///
    /// Note: in Kaspa DAG consensus, a block's transactions are accepted by the next chain block.
    /// The caller must mine an additional block for the transactions to be accepted.
    pub async fn mine_block<T: BorshSerialize>(&self, txs: Option<&[T]>) -> Hash {
        let mut template =
            self.grpc_client.get_block_template(self.address.clone(), vec![]).await.unwrap();

        if let Some(txs) = txs {
            let payloads: Vec<Vec<u8>> = txs.iter().map(|tx| borsh::to_vec(tx).unwrap()).collect();
            let signed_txs = self.build_payload_transactions(payloads).await;
            for tx in &signed_txs {
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
            hashes.push(self.mine_block::<u8>(None).await);
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
        self.grpc_client.disconnect().await.unwrap();
    }

    /// Builds signed L1 transactions, each carrying the given payload.
    ///
    /// Requires enough spendable UTXOs (call [`mine_utxos`](Self::mine_utxos) first).
    async fn build_payload_transactions(&self, payloads: Vec<Vec<u8>>) -> Vec<Transaction> {
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
                let fee = 10 * (200 + 34 + 1000 + payload.len() as u64);
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
                            TX_VERSION,
                            vec![input],
                            vec![output],
                            0,
                            SUBNETWORK_ID_NATIVE, // TODO: Discuss if necessary to identify
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
            .tap_mut(|utxos| utxos.sort_by(|a, b| b.1.amount.cmp(&a.1.amount)))
    }
}
