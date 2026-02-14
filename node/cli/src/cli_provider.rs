use clap::parser::ValueSource::CommandLine;
use figment::{
    Metadata, Profile, Provider,
    providers::Serialized,
    value::{Dict, Map, Value},
};
use tap::Tap;

use crate::params::NodeParams;

/// Figment provider that only includes CLI arguments explicitly set by the user.
///
/// Clap populates default values for every declared argument, so naively serializing `NodeParams`
/// would override TOML and environment layers. This provider solves that by serializing the full
/// params struct, then filtering the resulting dict to only retain values whose corresponding clap
/// arg has `ValueSource::CommandLine`.
pub struct CliProvider(Dict);

impl CliProvider {
    /// Serializes `params` into a figment dict, then filters it to only retain values
    /// whose corresponding clap arg was explicitly set on the command line.
    pub fn new(matches: &clap::ArgMatches, params: &NodeParams) -> Self {
        // Serialize all params (including clap defaults) into a figment dict, then strip it
        // down to only the values the user actually passed on the command line.
        Self(Self::filter(
            matches,
            &Serialized::defaults(params)
                .data()
                .expect("params serialization failed")
                .remove(&Profile::Default)
                .unwrap_or_default(),
            "",
        ))
    }

    /// Recursively filters a dict to only include values explicitly set on the command line.
    ///
    /// Clap arg names are derived from the dict path by joining nested keys with `-` and
    /// replacing underscores with hyphens (e.g. `l1_bridge.connect_timeout_ms` becomes
    /// `l1-bridge-connect-timeout-ms`). This convention must match the `#[arg(long = "...")]`
    /// declarations on the param structs.
    fn filter(matches: &clap::ArgMatches, dict: &Dict, prefix: &str) -> Dict {
        Dict::new().tap_mut(|filtered| {
            for (key, value) in dict {
                let segment = key.replace('_', "-");
                let arg =
                    if prefix.is_empty() { segment.clone() } else { format!("{prefix}-{segment}") };
                match value {
                    // Nested struct - recurse with the extended prefix.
                    Value::Dict(tag, nested) => {
                        let nested = Self::filter(matches, nested, &arg);
                        if !nested.is_empty() {
                            filtered.insert(key.clone(), Value::Dict(*tag, nested));
                        }
                    }
                    // Leaf value - only include if the user explicitly set it.
                    _ => {
                        if matches.value_source(arg.as_str()) == Some(CommandLine) {
                            filtered.insert(key.clone(), value.clone());
                        }
                    }
                }
            }
        })
    }
}

/// Exposes the filtered dict to figment under the default profile, tagged as "CLI" in
/// error messages and provenance tracking.
impl Provider for CliProvider {
    fn metadata(&self) -> Metadata {
        Metadata::named("CLI")
    }

    fn data(&self) -> Result<Map<Profile, Dict>, figment::Error> {
        Ok(Map::new().tap_mut(|m| {
            m.insert(Profile::Default, self.0.clone());
        }))
    }
}
