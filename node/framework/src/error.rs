/// Errors that can occur when communicating with the node worker via [`NodeApi`](crate::NodeApi).
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    /// The request channel is closed - the worker thread has stopped.
    #[error("node worker has stopped")]
    WorkerStopped,
    /// The request was sent but the worker dropped it without responding (e.g. during shutdown).
    #[error("request was dropped by the worker without a response")]
    RequestDropped,
}

/// Convenience alias for results returned by the node API.
pub type NodeResult<T> = Result<T, NodeError>;
