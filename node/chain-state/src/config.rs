/// Configuration for ChainState.
#[derive(Clone, Debug)]
pub struct ChainStateConfig {
    /// Size of each epoch for pruning (default: 1000).
    pub epoch_size: u64,
}

impl Default for ChainStateConfig {
    fn default() -> Self {
        Self { epoch_size: 1000 }
    }
}

impl ChainStateConfig {
    /// Creates a new configuration with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the epoch size for pruning.
    pub fn with_epoch_size(mut self, size: u64) -> Self {
        self.epoch_size = size;
        self
    }
}
