//! Layered runner configuration.
//!
//! The library consumes a fully-resolved, typed [`RunnerConfig`]. The binary builds one by layering
//! sources, highest precedence first: explicit CLI flags, then `VPRUN_*` environment variables, then
//! a `--config <path>` TOML file, then built-in defaults. Examples and tests construct a
//! [`RunnerConfig`] directly instead of layering.

use std::{path::PathBuf, str::FromStr};

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use kaspa_consensus_core::network::{NetworkId, NetworkType};
use kaspa_hashes::Hash;
use secp256k1::SecretKey;
use serde::{Deserialize, Serialize};

use crate::node::Elfs;

/// Explicit start mode. Where tn10-flow inferred this from persisted-or-env state, the runner makes
/// it a first-class choice; the safety guards (catch-up needs a deploy block, resume needs persisted
/// state) are preserved as typed errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum StartMode {
    /// Bootstrap a fresh covenant (dev-pins under dev mode, real-pins otherwise). Needs a clean data
    /// dir.
    Fresh,
    /// Reconstruct identity from the persisted state file and replay L1 from the persisted deploy
    /// block. Fails if there is no persisted state.
    Resume,
    /// Join an existing covenant (`covenant_id` + `start_from` deploy block). Fails fast without
    /// `start_from`.
    Catchup,
}

/// A fully-resolved, typed runner configuration. This is what the library consumes.
#[derive(Clone)]
pub struct RunnerConfig {
    /// Borsh wRPC URL of the remote node, e.g. `ws://1.2.3.4:17210`.
    pub wrpc_url: String,
    /// Fee / bootstrap key. Bootstrap (and, in the examples, action issuing) spend from it.
    pub private_key: SecretKey,
    /// Kaspa network to connect to. No default: the operator/example chooses it.
    pub network_id: NetworkId,
    /// Per-transaction program guest ELF (the arbitrary program under execution). Required by the
    /// path-based [`run`](crate::run) entry point; `None` when the caller supplies ELF bytes to
    /// [`start_runner`](crate::start_runner) directly (as the examples do).
    pub program_elf: Option<PathBuf>,
    /// Batch-processor guest ELF. See [`program_elf`](Self::program_elf).
    pub batch_elf: Option<PathBuf>,
    /// Batch-aggregator guest ELF. See [`program_elf`](Self::program_elf).
    pub aggregator_elf: Option<PathBuf>,
    /// RocksDB + state-file directory.
    pub data_dir: PathBuf,
    /// Lane id, if pinned. Resolved against storage in [`start`](crate::start) when `None`.
    pub lane_id: Option<u32>,
    /// Covenant id, if joining/resuming a known one.
    pub covenant_id: Option<Hash>,
    /// Bootstrap transaction id; the covenant UTXO's outpoint is `(this, 0)`.
    pub bootstrap_txid: Option<Hash>,
    /// Explicit L1 deploy block the bridge seeds its fresh-chain root at (catch-up).
    pub start_from: Option<Hash>,
    /// Chain-block head-room the bridge seeds below the sink for a fresh lane.
    pub seed_depth: u64,
    /// Run the proving + settlement path. Off = execution-only daemon.
    pub prove: bool,
    /// Explicit start mode, or `None` to auto-select (resume if the data dir is populated, else
    /// fresh).
    pub start_mode: Option<StartMode>,
}

/// ELF bytes loaded from [`RunnerConfig`]'s paths, kept alive so [`Elfs`] can borrow them.
pub struct OwnedElfs {
    program: Vec<u8>,
    batch: Vec<u8>,
    aggregator: Vec<u8>,
}

impl OwnedElfs {
    /// Borrows the three ELF byte slices for backend construction.
    pub fn as_elfs(&self) -> Elfs<'_> {
        Elfs { program: &self.program, batch: &self.batch, aggregator: &self.aggregator }
    }
}

impl RunnerConfig {
    /// Reads the three guest ELFs named by this config into memory. Errors if any path is unset
    /// (the path-based entry point requires all three).
    pub fn load_elfs(&self) -> Result<OwnedElfs, ConfigError> {
        let read = |p: &Option<PathBuf>, field: &'static str| {
            let p = p.as_ref().ok_or(ConfigError::Missing(field))?;
            std::fs::read(p).map_err(|e| ConfigError::Elf { path: p.clone(), source: e })
        };
        Ok(OwnedElfs {
            program: read(&self.program_elf, "program_elf")?,
            batch: read(&self.batch_elf, "batch_elf")?,
            aggregator: read(&self.aggregator_elf, "aggregator_elf")?,
        })
    }

    /// Layers CLI flags over `VPRUN_*` env vars over an optional TOML file over defaults, then
    /// resolves and validates. `cli` carries only the flags the operator actually set (unset ones are
    /// `None` and do not clobber lower layers).
    pub fn load(cli: RawConfig, config_path: Option<PathBuf>) -> Result<Self, ConfigError> {
        let mut figment = Figment::new();
        if let Some(path) = config_path {
            figment = figment.merge(Toml::file(path));
        }
        figment = figment.merge(Env::prefixed("VPRUN_")).merge(Serialized::defaults(cli));
        let raw: RawConfig =
            figment.extract().map_err(|e| ConfigError::Layering(Box::new(e)))?;
        raw.resolve()
    }
}

/// The unresolved, all-optional shape shared by every config layer (CLI, env, TOML file). Every
/// field is optional so a layer only overrides what it sets. Typed parsing and defaulting happen in
/// [`RawConfig::resolve`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RawConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wrpc_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program_elf: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_elf: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregator_elf: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lane_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covenant_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bootstrap_txid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed_depth: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prove: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_mode: Option<StartMode>,
}

impl RawConfig {
    /// Applies defaults and typed parsing, validating required fields.
    fn resolve(self) -> Result<RunnerConfig, ConfigError> {
        let wrpc_url = self.wrpc_url.ok_or(ConfigError::Missing("wrpc_url"))?;
        let private_key = {
            let hex = self.private_key.ok_or(ConfigError::Missing("private_key"))?;
            SecretKey::from_str(hex.trim())
                .map_err(|_| ConfigError::Invalid("private_key", "32-byte hex secp256k1 key"))?
        };
        let network_id = match self.network {
            Some(n) => parse_network(&n)?,
            None => return Err(ConfigError::Missing("network")),
        };
        let data_dir = self.data_dir.unwrap_or_else(|| PathBuf::from("./vprun-data"));

        Ok(RunnerConfig {
            wrpc_url,
            private_key,
            network_id,
            program_elf: self.program_elf,
            batch_elf: self.batch_elf,
            aggregator_elf: self.aggregator_elf,
            data_dir,
            lane_id: self.lane_id,
            covenant_id: parse_opt_hash(self.covenant_id, "covenant_id")?,
            bootstrap_txid: parse_opt_hash(self.bootstrap_txid, "bootstrap_txid")?,
            start_from: parse_opt_hash(self.start_from, "start_from")?,
            seed_depth: self.seed_depth.unwrap_or(100),
            prove: self.prove.unwrap_or(false),
            start_mode: self.start_mode,
        })
    }
}

/// Parses an optional 32-byte hex value into a [`Hash`].
fn parse_opt_hash(v: Option<String>, field: &'static str) -> Result<Option<Hash>, ConfigError> {
    v.map(|s| Hash::from_str(s.trim()).map_err(|_| ConfigError::Invalid(field, "32-byte hex")))
        .transpose()
}

/// Parses a network name, case-insensitive and trimmed. Accepts `mainnet`, `testnet-N` (including
/// `testnet-10`/`testnet10`/`tn10`), `devnet`, `simnet`.
pub fn parse_network(raw: &str) -> Result<NetworkId, ConfigError> {
    let v = raw.trim().to_lowercase();
    let id = match v.as_str() {
        "testnet-10" | "testnet10" | "tn10" => NetworkId::with_suffix(NetworkType::Testnet, 10),
        "mainnet" => NetworkId::new(NetworkType::Mainnet),
        "devnet" => NetworkId::new(NetworkType::Devnet),
        "simnet" => NetworkId::new(NetworkType::Simnet),
        _ => match v.strip_prefix("testnet-").and_then(|n| n.parse::<u32>().ok()) {
            Some(n) => NetworkId::with_suffix(NetworkType::Testnet, n),
            None => {
                return Err(ConfigError::Invalid(
                    "network",
                    "mainnet | testnet-N | tn10 | devnet | simnet",
                ));
            }
        },
    };
    Ok(id)
}

/// Configuration resolution failure.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A required field was not supplied by any layer.
    #[error("missing required config field: {0}")]
    Missing(&'static str),
    /// A field was supplied but malformed. The second field names the expected shape.
    #[error("invalid config field {0}: expected {1}")]
    Invalid(&'static str, &'static str),
    /// A guest ELF named by the config could not be read.
    #[error("failed to read ELF {path}: {source}")]
    Elf {
        /// The path that failed to read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// figment failed to merge/extract the layered sources.
    #[error("config layering error: {0}")]
    Layering(#[source] Box<figment::Error>),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid non-zero secp256k1 secret key in hex, for tests that need `resolve` to succeed.
    const KEY_HEX: &str = "0101010101010101010101010101010101010101010101010101010101010101";

    fn minimal_raw() -> RawConfig {
        RawConfig {
            wrpc_url: Some("ws://file".into()),
            private_key: Some(KEY_HEX.into()),
            network: Some("tn10".into()),
            program_elf: Some("p.elf".into()),
            batch_elf: Some("b.elf".into()),
            aggregator_elf: Some("a.elf".into()),
            ..Default::default()
        }
    }

    #[test]
    fn network_parsing() {
        assert_eq!(parse_network("tn10").unwrap(), NetworkId::with_suffix(NetworkType::Testnet, 10));
        assert_eq!(
            parse_network(" Testnet-42 ").unwrap(),
            NetworkId::with_suffix(NetworkType::Testnet, 42)
        );
        assert_eq!(parse_network("mainnet").unwrap(), NetworkId::new(NetworkType::Mainnet));
        assert!(matches!(parse_network("bogus"), Err(ConfigError::Invalid("network", _))));
    }

    #[test]
    fn resolve_reports_missing_required() {
        assert!(matches!(RawConfig::default().resolve(), Err(ConfigError::Missing("wrpc_url"))));
    }

    #[test]
    fn load_elfs_reports_missing_path() {
        // ELF paths are optional at resolve time (byte-based callers skip them) but required by the
        // path-based `load_elfs`.
        let cfg = RawConfig { program_elf: None, ..minimal_raw() }.resolve().unwrap();
        assert!(matches!(cfg.load_elfs(), Err(ConfigError::Missing("program_elf"))));
    }

    #[test]
    fn resolve_applies_defaults() {
        let cfg = minimal_raw().resolve().unwrap();
        assert_eq!(cfg.seed_depth, 100);
        assert!(!cfg.prove);
        assert_eq!(cfg.data_dir, PathBuf::from("./vprun-data"));
        assert_eq!(cfg.start_mode, None);
    }

    #[test]
    fn resolve_rejects_bad_hash() {
        let bad = RawConfig { covenant_id: Some("nothex".into()), ..minimal_raw() };
        assert!(matches!(bad.resolve(), Err(ConfigError::Invalid("covenant_id", _))));
    }

    #[test]
    fn layering_precedence_flag_over_env_over_file() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "cfg.toml",
                &format!(
                    "wrpc_url = \"ws://file\"\nprivate_key = \"{KEY_HEX}\"\nnetwork = \"tn10\"\n\
                     program_elf = \"p.elf\"\nbatch_elf = \"b.elf\"\naggregator_elf = \"a.elf\"\n\
                     seed_depth = 1\n"
                ),
            )?;

            // File only: seed_depth comes from the file.
            let file_only =
                RunnerConfig::load(RawConfig::default(), Some("cfg.toml".into())).unwrap();
            assert_eq!(file_only.seed_depth, 1);
            assert_eq!(file_only.wrpc_url, "ws://file");

            // Env beats file.
            jail.set_env("VPRUN_SEED_DEPTH", "2");
            let env_over_file =
                RunnerConfig::load(RawConfig::default(), Some("cfg.toml".into())).unwrap();
            assert_eq!(env_over_file.seed_depth, 2);

            // Explicit flag beats env and file.
            let cli = RawConfig { seed_depth: Some(3), ..Default::default() };
            let flag_over_all = RunnerConfig::load(cli, Some("cfg.toml".into())).unwrap();
            assert_eq!(flag_over_all.seed_depth, 3);
            // Unset flag fields still fall through to the file.
            assert_eq!(flag_over_all.wrpc_url, "ws://file");
            Ok(())
        });
    }
}
