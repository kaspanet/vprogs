use clap::parser::{ValueSource, ValueSource::CommandLine};
use figment::{
    Metadata, Profile,
    value::{Dict, Map, Value},
};
use tap::Tap;

use crate::params::NodeParams;

/// Figment provider that only includes CLI arguments explicitly set by the user.
/// This prevents clap's default values from overriding TOML and environment layers.
pub struct CliProvider(Dict);

impl CliProvider {
    pub fn from_matches(matches: &clap::ArgMatches, params: &NodeParams) -> Self {
        Self(Dict::new()).tap_mut(|p| {
            p.put(
                matches,
                &[
                    ("log_level", params.log_level.clone().into()),
                    ("api_channel_capacity", params.api_channel_capacity.into()),
                ],
            );
            p.section(
                matches,
                "execution",
                &[("worker_count", params.execution.worker_count.into())],
            );
            p.section(
                matches,
                "storage",
                &[
                    ("data_dir", params.storage.data_dir.display().to_string().into()),
                    ("read_worker_count", params.storage.read_worker_count.into()),
                    ("read_buffer_depth", params.storage.read_buffer_depth.into()),
                    ("write_batch_size", params.storage.write_batch_size.into()),
                    ("write_batch_duration_ms", params.storage.write_batch_duration_ms.into()),
                ],
            );
            p.section(
                matches,
                "l1_bridge",
                &[
                    ("url", params.l1_bridge.url.clone().unwrap_or_default().into()),
                    ("network_id", params.l1_bridge.network_id.clone().into()),
                    ("connect_timeout_ms", params.l1_bridge.connect_timeout_ms.into()),
                    ("connect_strategy", params.l1_bridge.connect_strategy.clone().into()),
                    ("filter_half_life_secs", params.l1_bridge.filter_half_life_secs.into()),
                ],
            );
        })
    }

    fn put(&mut self, matches: &clap::ArgMatches, entries: &[(&str, Value)]) {
        for (key, val) in entries {
            if matches.value_source(key.replace('_', "-").as_str()) == Some(CommandLine) {
                self.0.insert((*key).into(), val.clone());
            }
        }
    }

    /// Inserts a nested dict for `name`, auto-prefixing each arg with `name` (underscores →
    /// hyphens).
    fn section(&mut self, matches: &clap::ArgMatches, name: &str, entries: &[(&str, Value)]) {
        let prefix = name.replace('_', "-");
        let mut nested = Dict::new();
        for (field, val) in entries {
            let arg = format!("{prefix}-{}", field.replace('_', "-"));
            if matches.value_source(arg.as_str()) == Some(ValueSource::CommandLine) {
                nested.insert((*field).into(), val.clone());
            }
        }
        if !nested.is_empty() {
            self.0.insert(name.into(), nested.into());
        }
    }
}

impl figment::Provider for CliProvider {
    fn metadata(&self) -> Metadata {
        Metadata::named("CLI")
    }

    fn data(&self) -> Result<Map<Profile, Dict>, figment::Error> {
        Ok(Map::new().tap_mut(|m| {
            m.insert(Profile::Default, self.0.clone());
        }))
    }
}
