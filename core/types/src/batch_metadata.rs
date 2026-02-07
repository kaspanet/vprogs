pub trait BatchMetadata: Default + Send + Sync + 'static {
    fn batch_id(&self) -> [u8; 32];
}

impl BatchMetadata for () {
    fn batch_id(&self) -> [u8; 32] {
        [0u8; 32]
    }
}
