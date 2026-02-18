use crate::VmInterface;

/// Configuration for the scheduler's execution environment.
#[derive(Clone, Debug)]
pub struct ExecutionConfig<V: VmInterface> {
    pub(crate) worker_count: usize,
    pub(crate) vm: Option<V>,
}

impl<V: VmInterface> ExecutionConfig<V> {
    /// Creates a new configuration with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the number of execution worker threads.
    pub fn with_worker_count(mut self, workers: usize) -> Self {
        self.worker_count = workers;
        self
    }

    /// Sets the VM implementation used to execute transactions.
    pub fn with_vm(mut self, vm: V) -> Self {
        self.vm = Some(vm);
        self
    }

    /// Consumes the config, returning the worker count and VM. Panics if no VM was set.
    pub fn unpack(mut self) -> (usize, V) {
        (self.worker_count, self.vm.take().expect("unpack requires vm to be set"))
    }
}

impl<V: VmInterface> Default for ExecutionConfig<V> {
    fn default() -> Self {
        Self { worker_count: num_cpus::get_physical(), vm: None }
    }
}
