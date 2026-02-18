use std::thread::JoinHandle;

use crate::{Batch, WorkersApi, task::Task};

/// Thread pool for parallel task execution using work-stealing.
///
/// Workers pull tasks from batch queues and a global task queue, stealing from
/// each other when idle.
pub struct ExecutionWorkers<T: Task, B: Batch<T>> {
    workers: WorkersApi<T, B>,
    handles: Vec<JoinHandle<()>>,
}

impl<T: Task, B: Batch<T>> ExecutionWorkers<T, B> {
    /// Creates a new thread pool with the given number of worker threads.
    pub fn new(worker_count: usize) -> Self {
        let (workers, handles) = WorkersApi::new_with_workers(worker_count);
        Self { workers, handles }
    }

    /// Submits a batch for execution, distributing it to all workers.
    pub fn execute(&self, batch: B) {
        self.workers.push_batch(batch);
    }

    /// Submits a standalone task to the global queue for execution by the next idle worker.
    pub fn submit_task(&self, task: T) {
        self.workers.push_task(task);
    }

    /// Signals all workers to stop and waits for them to finish.
    pub fn shutdown(self) {
        self.workers.shutdown();
        for handle in self.handles {
            handle.join().expect("executor worker panicked");
        }
    }
}
