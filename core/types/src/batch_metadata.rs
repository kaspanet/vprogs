use std::fmt::Debug;

/// Opaque metadata attached to each scheduler batch, supporting serialization.
///
/// Implementors provide a unique identifier and a byte-level round-trip so the
/// scheduler can persist and restore metadata without knowing its concrete type.
pub trait BatchMetadata: Clone + Debug + Default + Send + Sync + 'static {
    /// Returns a unique 32-byte identifier for this batch.
    fn id(&self) -> [u8; 32];

    /// Serializes the metadata into a byte vector.
    fn to_bytes(&self) -> Vec<u8>;

    /// Deserializes metadata from the given byte slice.
    fn from_bytes(bytes: &[u8]) -> Self;
}
