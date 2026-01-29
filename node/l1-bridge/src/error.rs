use thiserror::Error;

/// Errors that can occur in the L1 bridge.
#[derive(Error, Debug)]
pub enum L1BridgeError {
    #[error("RPC call failed: {0}")]
    Rpc(String),
}

/// Result type for L1 bridge operations.
pub type Result<T> = std::result::Result<T, L1BridgeError>;
