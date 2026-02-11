use std::{
    error::Error,
    fmt,
    fmt::{Display, Formatter},
};

/// Errors produced by the scheduler.
#[derive(Debug, Clone, Copy)]
pub enum SchedulerError {
    /// Pruning has advanced past the rollback target. The rollback pointers
    /// needed to restore state have been deleted.
    PruningConflict,
}

impl Display for SchedulerError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::PruningConflict => write!(
                f,
                "pruning has advanced past the rollback target: \
                 rollback pointers needed to restore state have been deleted"
            ),
        }
    }
}

impl Error for SchedulerError {}

/// Convenience alias for results returned by the scheduler.
pub type SchedulerResult<T> = Result<T, SchedulerError>;
