#[derive(Clone, Debug)]
pub struct ReadConfig {
    pub max_readers: usize,
    pub buffer_depth_per_reader: usize,
}

impl Default for ReadConfig {
    fn default() -> Self {
        Self { max_readers: 8, buffer_depth_per_reader: 128 }
    }
}

impl ReadConfig {
    pub fn with_max_readers(mut self, max_readers: usize) -> Self {
        self.max_readers = max_readers;
        self
    }

    pub fn with_buffer_depth_per_reader(mut self, buffer_depth_per_reader: usize) -> Self {
        self.buffer_depth_per_reader = buffer_depth_per_reader;
        self
    }
}
