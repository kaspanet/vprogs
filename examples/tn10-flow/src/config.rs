//! Environment-driven configuration. A proper CLI is future work; for the POC every knob is an
//! env var. Required: `TN10_WRPC_URL`, `TN10_PRIVATE_KEY`.

use std::{path::PathBuf, str::FromStr};

use kaspa_consensus_core::network::{NetworkId, NetworkType};
use kaspa_hashes::Hash;
use secp256k1::SecretKey;

/// Fully-parsed runtime configuration.
pub struct Config {
    /// Borsh wRPC URL of the remote node, e.g. `ws://1.2.3.4:17210`. Used by the bridge and for
    /// transaction submission.
    pub wrpc_url: String,
    /// Issuer / fee key. Required: bootstrap and activity both spend from it.
    pub private_key: SecretKey,
    /// Lane id from env, if any. Final value is resolved against storage in `main`.
    pub lane_id_env: Option<u32>,
    /// Covenant id from env, if any. Final value is resolved against storage in `main`.
    pub covenant_id_env: Option<Hash>,
    /// Bootstrap transaction id from env, if any. The covenant UTXO's outpoint is `(this, 0)`.
    /// Required alongside `covenant_id_env` to catch up to a never-settled covenant: the outpoint
    /// is not derivable from the covenant id.
    pub bootstrap_txid: Option<Hash>,
    /// Explicit L1 block the bridge seeds its fresh-chain root at (a covenant's deploy block), so
    /// a catch-up node rebuilds state forward from there. `None` defers to
    /// `seed_depth`/pruning point.
    pub start_from: Option<Hash>,
    /// RocksDB + state-file directory.
    pub data_dir: PathBuf,
    /// Milliseconds between issued activity transactions.
    pub activity_interval_ms: u64,
    /// Total activity transactions to issue (0 = unbounded).
    pub activity_count: u64,
    /// Run the proving + settlement path (`TN10_SETTLE=1`): bootstrap a real-pins covenant, prove
    /// each bundle, and settle it on chain. Needs real proofs (the `cuda` build without
    /// `RISC0_DEV_MODE`); the default is the execution-only daemon. See `main.rs`.
    pub enable_settlements: bool,
    /// Chain-block head-room the bridge seeds below the sink, so a freshly bootstrapped lane
    /// starts near the tip instead of replaying the whole pruning window. Must exceed the
    /// deepest reorg the node produces, or the bridge panics rolling back past its root.
    pub seed_depth: u64,
    /// Kaspa network to connect to.
    pub network_id: NetworkId,
}

impl Config {
    /// Reads configuration from the environment, panicking with a clear message on missing or
    /// malformed required values.
    pub fn from_env() -> Self {
        let wrpc_url = req("TN10_WRPC_URL");

        let private_key = {
            let hex = req("TN10_PRIVATE_KEY");
            SecretKey::from_str(hex.trim())
                .expect("TN10_PRIVATE_KEY must be a 32-byte hex secp256k1 key")
        };

        let lane_id_env =
            opt("TN10_LANE_ID").map(|s| s.parse().expect("TN10_LANE_ID must be a u32"));
        let covenant_id_env = opt("TN10_COVENANT_ID")
            .map(|s| Hash::from_str(s.trim()).expect("TN10_COVENANT_ID must be 32-byte hex"));
        let bootstrap_txid = opt("TN10_BOOTSTRAP_TXID")
            .map(|s| Hash::from_str(s.trim()).expect("TN10_BOOTSTRAP_TXID must be 32-byte hex"));
        let start_from = opt("TN10_START_FROM")
            .map(|s| Hash::from_str(s.trim()).expect("TN10_START_FROM must be 32-byte hex"));

        let data_dir = PathBuf::from(opt("TN10_DATA_DIR").unwrap_or_else(|| "./tn10-data".into()));

        Self {
            wrpc_url,
            private_key,
            lane_id_env,
            covenant_id_env,
            bootstrap_txid,
            start_from,
            data_dir,
            activity_interval_ms: opt_u64("TN10_ACTIVITY_INTERVAL_MS", 5_000),
            activity_count: opt_u64("TN10_ACTIVITY_COUNT", 0),
            enable_settlements: opt("TN10_SETTLE").is_some_and(|s| s != "0"),
            seed_depth: opt_u64("TN10_SEED_DEPTH", 100),
            network_id: opt("TN10_NETWORK").map(|s| parse_network(&s)).unwrap_or(TESTNET_10),
        }
    }
}

/// Default network when `TN10_NETWORK` is unset.
const TESTNET_10: NetworkId = NetworkId::with_suffix(NetworkType::Testnet, 10);

/// Parses `TN10_NETWORK`, panicking on an unrecognized value. Accepts (case-insensitive, trimmed):
/// `testnet-10`/`testnet10`/`tn10`, `testnet-N` (any suffix), `devnet`, `simnet`, `mainnet`.
fn parse_network(raw: &str) -> NetworkId {
    let v = raw.trim().to_lowercase();
    match v.as_str() {
        "testnet-10" | "testnet10" | "tn10" => TESTNET_10,
        "devnet" => NetworkId::new(NetworkType::Devnet),
        "simnet" => NetworkId::new(NetworkType::Simnet),
        "mainnet" => NetworkId::new(NetworkType::Mainnet),
        _ => {
            let suffix = v.strip_prefix("testnet-").and_then(|n| n.parse::<u32>().ok());
            match suffix {
                Some(n) => NetworkId::with_suffix(NetworkType::Testnet, n),
                None => panic!(
                    "TN10_NETWORK={raw:?} unrecognized; expected one of: \
                     testnet-10|testnet10|tn10, testnet-N, devnet, simnet, mainnet"
                ),
            }
        }
    }
}

fn req(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("missing required env var {key}"))
}

fn opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

fn opt_u64(key: &str, default: u64) -> u64 {
    opt(key).map(|s| s.parse().unwrap_or_else(|_| panic!("{key} must be a u64"))).unwrap_or(default)
}
