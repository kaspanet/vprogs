/// Errors produced by the scheduler.
#[derive(Debug, Clone, Copy, thiserror::Error)]
pub enum SchedulerError {
    /// Pruning has advanced past the rollback target. The rollback pointers
    /// needed to restore state have been deleted.
    #[error("pruning has advanced past the rollback target")]
    PruningConflict,
}

/// Convenience alias for results returned by the scheduler.
pub type SchedulerResult<T> = Result<T, SchedulerError>;
