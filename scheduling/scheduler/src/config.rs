/// Configuration for the scheduler's execution environment.
#[derive(Clone, Debug)]
pub struct ExecutionConfig<P> {
    /// Number of parallel execution worker threads.
    pub worker_count: usize,
    /// The processor used to execute transactions.
    pub processor: Option<P>,
}

impl<P> Default for ExecutionConfig<P> {
    fn default() -> Self {
        Self { worker_count: num_cpus::get_physical(), processor: None }
    }
}

impl<P> ExecutionConfig<P> {
    /// Sets the number of parallel execution worker threads.
    pub fn with_worker_count(mut self, workers: usize) -> Self {
        self.worker_count = workers;
        self
    }

    /// Sets the processor used to execute transactions.
    pub fn with_processor(mut self, processor: P) -> Self {
        self.processor = Some(processor);
        self
    }

    /// Consumes the config, returning the worker count and processor. Panics if no processor
    /// was set.
    pub fn unpack(mut self) -> (usize, P) {
        (self.worker_count, self.processor.take().expect("unpack requires processor to be set"))
    }
}
