use std::time::Duration;

#[derive(Clone, Debug)]
pub struct WriteConfig {
    pub batch_size: usize,
    pub batch_duration: Duration,
}

impl Default for WriteConfig {
    fn default() -> Self {
        Self { batch_size: 1000, batch_duration: Duration::from_millis(10) }
    }
}

impl WriteConfig {
    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size;
        self
    }

    pub fn with_batch_duration(mut self, batch_duration: Duration) -> Self {
        self.batch_duration = batch_duration;
        self
    }
}
