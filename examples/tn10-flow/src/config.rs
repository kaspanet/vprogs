//! Environment-driven configuration for the tn10-flow example. A proper CLI lives in the `vprun`
//! binary; this POC keeps its historical `TN10_*` env surface and maps it onto a
//! [`RunnerConfig`](vprogs_runner::RunnerConfig), adding the example-only activity-issuer knobs the
//! generic runner does not carry.

use std::{path::PathBuf, str::FromStr};

use kaspa_consensus_core::network::{NetworkId, NetworkType};
use kaspa_hashes::Hash;
use secp256k1::SecretKey;
use vprogs_runner::{RunnerConfig, StartMode};

/// Default network when `TN10_NETWORK` is unset.
const TESTNET_10: NetworkId = NetworkId::with_suffix(NetworkType::Testnet, 10);

/// Parsed tn10-flow configuration: the generic [`RunnerConfig`] plus the activity issuer's knobs.
pub struct Config {
    /// The engine configuration handed to the runner.
    pub runner: RunnerConfig,
    /// Milliseconds between issued activity transactions.
    pub activity_interval_ms: u64,
    /// Total activity transactions to issue (0 = unbounded).
    pub activity_count: u64,
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
        let lane_id = opt("TN10_LANE_ID").map(|s| s.parse().expect("TN10_LANE_ID must be a u32"));
        let covenant_id = opt("TN10_COVENANT_ID")
            .map(|s| Hash::from_str(s.trim()).expect("TN10_COVENANT_ID must be 32-byte hex"));
        let bootstrap_txid = opt("TN10_BOOTSTRAP_TXID")
            .map(|s| Hash::from_str(s.trim()).expect("TN10_BOOTSTRAP_TXID must be 32-byte hex"));
        let start_from = opt("TN10_START_FROM")
            .map(|s| Hash::from_str(s.trim()).expect("TN10_START_FROM must be 32-byte hex"));
        let data_dir = PathBuf::from(opt("TN10_DATA_DIR").unwrap_or_else(|| "./tn10-data".into()));
        let network_id = opt("TN10_NETWORK").map(|s| parse_network(&s)).unwrap_or(TESTNET_10);

        // An env-supplied covenant id means "join that covenant" (catch-up); otherwise let the
        // runner auto-select (resume if the data dir is populated, else fresh bootstrap). This
        // mirrors the POC's original storage > env > bootstrap precedence.
        let start_mode = covenant_id.map(|_| StartMode::Catchup);

        let runner = RunnerConfig {
            wrpc_url,
            private_key,
            network_id,
            // The example loads its guest ELFs via the test-suite loaders and hands the bytes to
            // the runner directly, so it carries no ELF paths.
            program_elf: None,
            batch_elf: None,
            aggregator_elf: None,
            data_dir,
            lane_id,
            covenant_id,
            bootstrap_txid,
            start_from,
            seed_depth: opt_u64("TN10_SEED_DEPTH", 500),
            prove: opt("TN10_SETTLE").is_some_and(|s| s != "0"),
            start_mode,
        };

        Self {
            runner,
            activity_interval_ms: opt_u64("TN10_ACTIVITY_INTERVAL_MS", 5_000),
            activity_count: opt_u64("TN10_ACTIVITY_COUNT", 0),
        }
    }
}

/// Parses `TN10_NETWORK`, panicking on an unrecognized value. Delegates to the runner's parser.
fn parse_network(raw: &str) -> NetworkId {
    vprogs_runner::parse_network(raw)
        .unwrap_or_else(|e| panic!("TN10_NETWORK={raw:?} unrecognized: {e}"))
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
