use crate::VmInterface;

#[derive(Clone, Debug)]
pub struct ExecutionConfig<V: VmInterface> {
    pub worker_count: usize,
    pub vm: Option<V>,
}

impl<V: VmInterface> Default for ExecutionConfig<V> {
    fn default() -> Self {
        Self { worker_count: num_cpus::get_physical(), vm: None }
    }
}

impl<V: VmInterface> ExecutionConfig<V> {
    pub fn with_worker_count(mut self, workers: usize) -> Self {
        self.worker_count = workers;
        self
    }

    pub fn with_vm(mut self, vm: V) -> Self {
        self.vm = Some(vm);
        self
    }
}
