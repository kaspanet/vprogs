/// Abstraction over a zero-knowledge backend's execution capabilities.
///
/// Implementations own their ELF binaries and receive pre-encoded wire bytes. Execution is
/// synchronous (called on scheduler worker threads).
pub trait ExecutionBackend: Clone + Send + Sync + 'static {
    /// Execute a transaction from pre-encoded wire bytes.
    /// Returns the raw execution result bytes.
    fn execute_transaction(&self, wire_bytes: &[u8]) -> Vec<u8>;
}
