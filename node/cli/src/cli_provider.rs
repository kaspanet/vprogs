use clap::parser::ValueSource;
use figment::{
    Metadata, Profile,
    value::{Dict, Map},
};

use crate::params::NodeParams;

/// Figment provider that only includes CLI arguments explicitly set by the user.
/// This prevents clap's default values from overriding TOML and environment layers.
pub struct CliOverrides(Dict);

impl CliOverrides {
    pub fn from_matches(matches: &clap::ArgMatches, params: &NodeParams) -> Self {
        let mut dict = Dict::new();

        // Top-level params.
        if set(matches, "log-level") {
            dict.insert("log_level".into(), params.log_level.clone().into());
        }
        if set(matches, "api-channel-capacity") {
            dict.insert(
                "api_channel_capacity".into(),
                (params.api_channel_capacity as u128).into(),
            );
        }

        // Execution params.
        let mut execution = Dict::new();
        if set(matches, "execution-worker-count") {
            execution.insert("worker_count".into(), (params.execution.worker_count as u128).into());
        }
        if !execution.is_empty() {
            dict.insert("execution".into(), execution.into());
        }

        // Storage params.
        let mut storage = Dict::new();
        if set(matches, "storage-data-dir") {
            storage.insert("data_dir".into(), params.storage.data_dir.display().to_string().into());
        }
        if set(matches, "storage-read-worker-count") {
            storage.insert("read_workers".into(), (params.storage.read_workers as u128).into());
        }
        if set(matches, "storage-read-buffer-depth") {
            storage.insert(
                "read_buffer_depth".into(),
                (params.storage.read_buffer_depth as u128).into(),
            );
        }
        if set(matches, "storage-write-batch-size") {
            storage.insert(
                "write_batch_size".into(),
                (params.storage.write_batch_size as u128).into(),
            );
        }
        if set(matches, "storage-write-batch-duration-ms") {
            storage.insert(
                "write_batch_duration_ms".into(),
                (params.storage.write_batch_duration_ms as u128).into(),
            );
        }
        if !storage.is_empty() {
            dict.insert("storage".into(), storage.into());
        }

        // L1 Bridge params.
        let mut l1_bridge = Dict::new();
        if set(matches, "l1-bridge-url") {
            if let Some(url) = &params.l1_bridge.url {
                l1_bridge.insert("url".into(), url.clone().into());
            }
        }
        if set(matches, "l1-bridge-network-id") {
            l1_bridge.insert("network_id".into(), params.l1_bridge.network_id.clone().into());
        }
        if set(matches, "l1-bridge-connect-timeout-ms") {
            l1_bridge.insert(
                "connect_timeout_ms".into(),
                (params.l1_bridge.connect_timeout_ms as u128).into(),
            );
        }
        if set(matches, "l1-bridge-connect-strategy") {
            l1_bridge.insert(
                "connect_strategy".into(),
                params.l1_bridge.connect_strategy.clone().into(),
            );
        }
        if set(matches, "l1-bridge-reorg-filter-halving-period-secs") {
            l1_bridge.insert(
                "reorg_filter_halving_period_secs".into(),
                (params.l1_bridge.reorg_filter_halving_period_secs as u128).into(),
            );
        }
        if !l1_bridge.is_empty() {
            dict.insert("l1_bridge".into(), l1_bridge.into());
        }

        Self(dict)
    }
}

fn set(matches: &clap::ArgMatches, arg: &str) -> bool {
    matches.value_source(arg) == Some(ValueSource::CommandLine)
}

impl figment::Provider for CliOverrides {
    fn metadata(&self) -> Metadata {
        Metadata::named("CLI")
    }

    fn data(&self) -> Result<Map<Profile, Dict>, figment::Error> {
        let mut map = Map::new();
        map.insert(Profile::Default, self.0.clone());
        Ok(map)
    }
}
