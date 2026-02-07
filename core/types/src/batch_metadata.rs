pub trait BatchMetadata: Default + Send + Sync + 'static {
    fn id(&self) -> [u8; 32];
    fn to_bytes(&self) -> Vec<u8>;
    fn from_bytes(bytes: &[u8]) -> Self;
}

impl BatchMetadata for () {
    fn id(&self) -> [u8; 32] {
        [0u8; 32]
    }

    fn to_bytes(&self) -> Vec<u8> {
        vec![]
    }

    fn from_bytes(_bytes: &[u8]) -> Self {}
}
