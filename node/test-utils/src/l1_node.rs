//! A single Kaspa L1 node for testing.

use std::{io::Write, time::Duration};

use kaspa_addresses::{Address, Prefix, Version};
use kaspa_consensus_core::{
    Hash,
    config::params::{OverrideParams, Params},
    header::Header,
    mass::units::ComputeBudget,
    merkle::calc_hash_merkle_root,
    network::{NetworkId, NetworkType},
    subnets::SubnetworkId,
    tx::{Transaction, UtxoEntry},
};
use kaspa_grpc_client::GrpcClient;
use kaspa_rpc_core::{RpcTransaction, api::rpc::RpcApi};
use kaspa_testing_integration::common::daemon::Daemon;
use kaspa_wrpc_server::address::WrpcNetAddress;
use kaspad_lib::args::Args;
use secp256k1::Keypair;
use vprogs_l1_wallet::Wallet;

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
    /// Creates and starts a new isolated node on `network`.
    ///
    /// Pass a customization closure to override consensus parameters. The closure receives the
    /// network's default [`Params`] to mutate. Pass `None` for vanilla defaults. The node's mining
    /// address and the [`Wallet`] view both take their prefix from `network`.
    ///
    /// ```no_run
    /// # use vprogs_node_test_utils::L1Node;
    /// # use kaspa_consensus_core::network::{NetworkId, NetworkType};
    /// # async fn example() {
    /// let simnet = NetworkId::new(NetworkType::Simnet);
    ///
    /// // Vanilla simnet defaults:
    /// let node = L1Node::new(simnet, None).await;
    ///
    /// // Fast coinbase maturity for UTXO tests:
    /// let node = L1Node::new(simnet, Some(|p| p.blockrate.coinbase_maturity = 10)).await;
    /// # }
    /// ```
    pub async fn new(network: NetworkId, customize: Option<fn(&mut Params)>) -> Self {
        kaspa_core::log::try_init_logger("INFO");

        // Generate a real keypair for signing transactions.
        let keypair = Keypair::new(secp256k1::SECP256K1, &mut secp256k1::rand::thread_rng());
        let (xonly, _parity) = keypair.x_only_public_key();
        let address =
            Address::new(Prefix::from(network.network_type()), Version::PubKey, &xonly.serialize());

        // Start from the network's default params and let the caller customize them.
        let mut params = Params::from(network);
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

        // Spawn the daemon with unsafe RPC, unsynced mining, and UTXO index enabled so we can mine
        // blocks without waiting for IBD and query UTXOs by address. The network flags pick which
        // genesis / consensus the daemon boots.
        let mut args = Args {
            unsafe_rpc: true,
            enable_unsynced_mining: true,
            disable_upnp: true,
            utxoindex: true,
            override_params_file: Some(
                params_file.path().to_str().expect("tempfile path must be utf-8").to_string(),
            ),
            ..Default::default()
        };
        match network.network_type() {
            NetworkType::Simnet => args.simnet = true,
            NetworkType::Testnet => {
                args.testnet = true;
                args.testnet_suffix = network.suffix.unwrap_or(0);
            }
            NetworkType::Devnet => args.devnet = true,
            NetworkType::Mainnet => {}
        }
        let mut daemon = Daemon::new_random_with_args(args, 10 /* fd budget */);
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

    /// A [`Wallet`] view over this node's client, params, and key. The wallet derives its address
    /// prefix from `params`, matching this node's network.
    fn wallet(&self) -> Wallet<'_, GrpcClient> {
        Wallet::new(&self.grpc_client, &self.params, self.keypair)
    }

    /// Builds signed L1 transactions, each carrying the given payload.
    ///
    /// Requires enough spendable UTXOs (call [`mine_utxos`](Self::mine_utxos) first).
    pub async fn build_payload_transactions(&self, payloads: Vec<Vec<u8>>) -> Vec<Transaction> {
        self.wallet().build_payload_transactions(payloads).await
    }

    /// Builds signed transactions tagged with the given `subnetwork_id` and `tx_version`. For
    /// native-subnetwork txs prefer [`Self::build_payload_transactions`].
    pub async fn build_subnet_payload_transactions(
        &self,
        payloads: Vec<Vec<u8>>,
        subnetwork_id: SubnetworkId,
        tx_version: u16,
    ) -> Vec<Transaction> {
        self.wallet().build_subnet_payload_transactions(payloads, subnetwork_id, tx_version).await
    }

    /// Funds and signs a settlement transaction without submitting it. The returned tx is ready to
    /// ship via [`Self::mine_block`] (deterministic) or [`Self::submit_settlement_transaction`]
    /// (mempool). See [`Wallet::prepare_settlement_transaction`].
    pub async fn prepare_settlement_transaction(
        &self,
        settlement_tx: Transaction,
        covenant_entry: UtxoEntry,
        covenant_compute_budget: ComputeBudget,
    ) -> Transaction {
        self.wallet()
            .prepare_settlement_transaction(settlement_tx, covenant_entry, covenant_compute_budget)
            .await
    }

    /// Funds, signs, and submits a settlement transaction through the mempool. Returns its id.
    pub async fn submit_settlement_transaction(
        &self,
        settlement_tx: Transaction,
        covenant_entry: UtxoEntry,
        covenant_compute_budget: ComputeBudget,
    ) -> Hash {
        let wallet = self.wallet();
        let tx = wallet
            .prepare_settlement_transaction(settlement_tx, covenant_entry, covenant_compute_budget)
            .await;
        wallet.submit_transaction(&tx).await.expect("settlement tx submission failed")
    }

    /// Builds a signed bootstrap transaction whose single output is P2SH(`redeem_script`) with a
    /// genesis covenant binding. Returns the tx and the covenant id consensus recomputes. See
    /// [`Wallet::build_covenant_bootstrap_transaction`].
    pub async fn build_covenant_bootstrap_transaction(
        &self,
        redeem_script: &[u8],
        value: u64,
    ) -> (Transaction, Hash) {
        self.wallet().build_covenant_bootstrap_transaction(redeem_script, value).await
    }
}
