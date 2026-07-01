//! Environment-driven configuration for the tn10-runtime example. Reuses the generic
//! [`RunnerConfig`](vprogs_runner::RunnerConfig) and adds the account-model driver's own knobs
//! (account count, per-account deposit, withdrawal amount). Env surface is `TN10RT_*`.

use std::{path::PathBuf, str::FromStr};

use kaspa_consensus_core::network::{NetworkId, NetworkType};
use kaspa_hashes::Hash;
use secp256k1::SecretKey;
use vprogs_runner::{RunnerConfig, StartMode};

/// Default network when `TN10RT_NETWORK` is unset.
const TESTNET_10: NetworkId = NetworkId::with_suffix(NetworkType::Testnet, 10);

/// Parsed tn10-runtime configuration: the generic [`RunnerConfig`] plus the driver's action knobs.
pub struct Config {
    /// The engine configuration handed to the runner.
    pub runner: RunnerConfig,
    /// Number of L2 accounts to create (each gets a deposit; the driver transfers and withdraws
    /// among the first two). Minimum 2.
    pub account_count: usize,
    /// Sompi deposited per account (must be at least the guest's creation minimum, 1000).
    pub deposit_amount: u64,
    /// Sompi transferred from account 0 to account 1.
    pub transfer_amount: u64,
    /// Sompi withdrawn from account 1; also written into config as the minimum withdrawal at Init.
    pub withdraw_amount: u64,
    /// Milliseconds to wait for L1 progress between scripted steps.
    pub step_delay_ms: u64,
}

impl Config {
    /// Reads configuration from the environment, panicking with a clear message on missing or
    /// malformed required values.
    pub fn from_env() -> Self {
        let wrpc_url = req("TN10RT_WRPC_URL");
        let private_key = {
            let hex = req("TN10RT_PRIVATE_KEY");
            SecretKey::from_str(hex.trim())
                .expect("TN10RT_PRIVATE_KEY must be a 32-byte hex secp256k1 key")
        };
        let lane_id =
            opt("TN10RT_LANE_ID").map(|s| s.parse().expect("TN10RT_LANE_ID must be a u32"));
        let covenant_id = opt("TN10RT_COVENANT_ID")
            .map(|s| Hash::from_str(s.trim()).expect("TN10RT_COVENANT_ID must be 32-byte hex"));
        let bootstrap_txid = opt("TN10RT_BOOTSTRAP_TXID")
            .map(|s| Hash::from_str(s.trim()).expect("TN10RT_BOOTSTRAP_TXID must be 32-byte hex"));
        let start_from = opt("TN10RT_START_FROM")
            .map(|s| Hash::from_str(s.trim()).expect("TN10RT_START_FROM must be 32-byte hex"));
        let data_dir =
            PathBuf::from(opt("TN10RT_DATA_DIR").unwrap_or_else(|| "./tn10-runtime-data".into()));
        let network_id = opt("TN10RT_NETWORK").map(|s| parse_network(&s)).unwrap_or(TESTNET_10);

        // An env-supplied covenant id means "join that covenant" (catch-up); otherwise let the
        // runner auto-select (resume if the data dir is populated, else fresh bootstrap).
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
            seed_depth: opt_u64("TN10RT_SEED_DEPTH", 200),
            prove: opt("TN10RT_SETTLE").is_some_and(|s| s != "0"),
            start_mode,
        };

        let account_count = opt_u64("TN10RT_ACCOUNTS", 2).max(2) as usize;

        Self {
            runner,
            account_count,
            deposit_amount: opt_u64("TN10RT_DEPOSIT_AMOUNT", 100_000_000),
            transfer_amount: opt_u64("TN10RT_TRANSFER_AMOUNT", 1_000),
            withdraw_amount: opt_u64("TN10RT_WITHDRAW_AMOUNT", 2_000),
            step_delay_ms: opt_u64("TN10RT_STEP_DELAY_MS", 5_000),
        }
    }
}

/// Parses `TN10RT_NETWORK`, panicking on an unrecognized value. Delegates to the runner's parser.
fn parse_network(raw: &str) -> NetworkId {
    vprogs_runner::parse_network(raw)
        .unwrap_or_else(|e| panic!("TN10RT_NETWORK={raw:?} unrecognized: {e}"))
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
