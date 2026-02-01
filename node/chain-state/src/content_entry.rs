/// Content entry stored in the ring buffer.
pub struct ContentEntry {
    /// The block's index.
    pub index: u64,
    /// Raw block data (opaque bytes for now, will be RpcBlock when integrated).
    pub data: Vec<u8>,
}
