//! Environment-driven configuration. A proper CLI is future work; for the POC every knob is an
//! env var. Required: `TN10_WRPC_URL`, `TN10_PRIVATE_KEY`.

use std::{path::PathBuf, str::FromStr};

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
    /// Batches bundled per proof / settlement when settling (`TN10_BUNDLE_SIZE`, default 10).
    pub bundle_size: usize,
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

        let data_dir = PathBuf::from(opt("TN10_DATA_DIR").unwrap_or_else(|| "./tn10-data".into()));

        Self {
            wrpc_url,
            private_key,
            lane_id_env,
            covenant_id_env,
            data_dir,
            activity_interval_ms: opt_u64("TN10_ACTIVITY_INTERVAL_MS", 5_000),
            activity_count: opt_u64("TN10_ACTIVITY_COUNT", 0),
            enable_settlements: opt("TN10_SETTLE").is_some_and(|s| s != "0"),
            bundle_size: opt_u64("TN10_BUNDLE_SIZE", 10) as usize,
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
