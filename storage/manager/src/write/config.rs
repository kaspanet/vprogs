use std::time::Duration;

#[derive(Clone, Debug)]
pub struct WriteConfig {
    pub max_batch_size: usize,
    pub max_batch_duration: Duration,
}

impl Default for WriteConfig {
    fn default() -> Self {
        Self { max_batch_size: 1000, max_batch_duration: Duration::from_millis(10) }
    }
}

impl WriteConfig {
    pub fn with_max_batch_size(mut self, max_batch_size: usize) -> Self {
        self.max_batch_size = max_batch_size;
        self
    }

    pub fn with_max_batch_duration(mut self, max_batch_duration: Duration) -> Self {
        self.max_batch_duration = max_batch_duration;
        self
    }
}
