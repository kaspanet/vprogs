use borsh::{BorshDeserialize, BorshSerialize};
use rkyv::{Archive, Deserialize, Serialize};

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Archive, Serialize, Deserialize)] // rkyv serialization
#[derive(BorshSerialize, BorshDeserialize)] // borsh serialization
pub struct ResourceId([u8; 32]);

impl ResourceId {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for ResourceId {
    fn from(bytes: [u8; 32]) -> Self {
        ResourceId(bytes)
    }
}

impl From<usize> for ResourceId {
    fn from(value: usize) -> Self {
        let mut bytes = [0u8; 32];
        bytes[24..32].copy_from_slice(&value.to_be_bytes());
        ResourceId(bytes)
    }
}
