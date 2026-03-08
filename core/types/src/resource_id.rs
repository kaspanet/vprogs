use borsh::{BorshDeserialize, BorshSerialize};
use rkyv::{Archive, Deserialize, Serialize};

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
#[derive(Archive, Serialize, Deserialize)] // rkyv serialization
#[derive(BorshSerialize, BorshDeserialize)] // borsh serialization
pub struct ResourceId([u8; 32]);

impl ResourceId {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Zero-copy view: reinterprets a `&[u8; 32]` as `&ResourceId`.
    ///
    /// Safe because `ResourceId` is `#[repr(transparent)]` over `[u8; 32]`.
    pub fn from_bytes_ref(bytes: &[u8; 32]) -> &ResourceId {
        // SAFETY: ResourceId is repr(transparent) over [u8; 32], so the layout is identical.
        unsafe { &*(bytes as *const [u8; 32] as *const ResourceId) }
    }
}

impl From<[u8; 32]> for ResourceId {
    fn from(bytes: [u8; 32]) -> Self {
        ResourceId(bytes)
    }
}
