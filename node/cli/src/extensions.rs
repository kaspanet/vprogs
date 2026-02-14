use vprogs_node_l1_bridge::ConnectStrategy;

/// Extends [`ConnectStrategy`] with string conversion (the upstream type lacks `Display`).
pub trait ConnectStrategyExt {
    /// Returns the lowercase string representation used by the CLI (e.g. `"retry"`, `"fallback"`).
    fn to_string(&self) -> String;
}

impl ConnectStrategyExt for ConnectStrategy {
    fn to_string(&self) -> String {
        match self {
            ConnectStrategy::Retry => "retry",
            ConnectStrategy::Fallback => "fallback",
        }
        .to_string()
    }
}
