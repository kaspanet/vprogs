/// Configuration for ChainState.
#[derive(Clone, Debug)]
pub struct ChainStateConfig {
    /// Size of content ring buffer (default: 128).
    pub content_buffer_capacity: usize,
    /// Size of each epoch for pruning (default: 1000).
    pub epoch_size: u64,
}

impl Default for ChainStateConfig {
    fn default() -> Self {
        Self { content_buffer_capacity: 128, epoch_size: 1000 }
    }
}

impl ChainStateConfig {
    /// Creates a new configuration with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the content buffer capacity.
    pub fn with_content_buffer_capacity(mut self, capacity: usize) -> Self {
        self.content_buffer_capacity = capacity;
        self
    }

    /// Sets the epoch size for pruning.
    pub fn with_epoch_size(mut self, size: u64) -> Self {
        self.epoch_size = size;
        self
    }
}
