use std::thread::JoinHandle;

use crate::{Batch, WorkersApi, task::Task};

pub struct ExecutionWorkers<T: Task, B: Batch<T>> {
    workers: WorkersApi<T, B>,
    handles: Vec<JoinHandle<()>>,
}

impl<T: Task, B: Batch<T>> ExecutionWorkers<T, B> {
    pub fn new(worker_count: usize) -> Self {
        let (workers, handles) = WorkersApi::new_with_workers(worker_count);
        Self { workers, handles }
    }

    pub fn execute(&self, batch: B) {
        self.workers.push_batch(batch);
    }

    pub fn submit_task(&self, task: T) {
        self.workers.push_task(task);
    }

    pub fn shutdown(self) {
        self.workers.shutdown();
        for handle in self.handles {
            handle.join().expect("executor worker panicked");
        }
    }
}
