#[derive(Clone, Debug)]
pub struct ReadConfig {
    pub worker_count: usize,
    pub worker_buffer_depth: usize,
}

impl Default for ReadConfig {
    fn default() -> Self {
        Self { worker_count: 8, worker_buffer_depth: 128 }
    }
}

impl ReadConfig {
    pub fn with_worker_count(mut self, worker_count: usize) -> Self {
        self.worker_count = worker_count;
        self
    }

    pub fn with_worker_buffer_depth(mut self, worker_buffer_depth: usize) -> Self {
        self.worker_buffer_depth = worker_buffer_depth;
        self
    }
}
