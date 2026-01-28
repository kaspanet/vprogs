use thiserror::Error;

/// Errors that can occur in the L1 bridge.
#[derive(Error, Debug)]
pub enum L1BridgeError {
    #[error("RPC connection error: {0}")]
    Connection(String),

    #[error("RPC call failed: {0}")]
    RpcCall(String),

    #[error("Subscription error: {0}")]
    Subscription(String),

    #[error("Block not found: {0}")]
    BlockNotFound(String),

    #[error("Event channel closed")]
    ChannelClosed,

    #[error("Bridge not connected")]
    NotConnected,

    #[error("Bridge already started")]
    AlreadyStarted,

    #[error("Bridge shutdown")]
    Shutdown,
}

/// Result type for L1 bridge operations.
pub type Result<T> = std::result::Result<T, L1BridgeError>;
