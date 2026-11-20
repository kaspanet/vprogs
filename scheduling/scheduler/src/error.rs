/// Errors produced by the scheduler.
#[derive(Debug, Clone, Copy, thiserror::Error)]
pub enum SchedulerError {
    /// Pruning has reached the rollback target.
    ///
    /// A pass has deleted the target's metadata or the rollback pointers needed to restore state,
    /// or has announced that it will.
    #[error("pruning has reached the rollback target")]
    PruningConflict,
    /// The rollback target has no batch on the canonical chain.
    ///
    /// Its batch was reorged away, or canceled before it committed, or the store holds no metadata
    /// for it.
    #[error("the rollback target is not a batch of this chain")]
    UnknownTarget,
}

/// Convenience alias for results returned by the scheduler.
pub type SchedulerResult<T> = Result<T, SchedulerError>;
