/// Minimal [`vprogs_core_types::BatchMetadata`] implementation.
///
/// Stores a single `u64` counter that is serialized as 8 big-endian bytes and
/// zero-padded to 32 bytes for the id.
#[derive(Clone, Debug, Default)]
pub struct BatchMetadata(u64);

impl BatchMetadata {
    /// Creates a new instance with the given counter value.
    pub fn new(counter: u64) -> Self {
        Self(counter)
    }
}

impl vprogs_core_types::BatchMetadata for BatchMetadata {
    fn id(&self) -> [u8; 32] {
        let mut id = [0u8; 32];
        id[..8].copy_from_slice(&self.0.to_be_bytes());
        id
    }

    fn to_bytes(&self) -> Vec<u8> {
        self.0.to_be_bytes().to_vec()
    }

    fn from_bytes(bytes: &[u8]) -> Self {
        Self(u64::from_be_bytes(bytes.try_into().expect("expected 8 bytes")))
    }
}
