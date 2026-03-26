use core::ops::Deref;

use borsh::{BorshDeserialize, BorshSerialize};
use rkyv::{Archive, Deserialize, Serialize};

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[derive(Archive, Serialize, Deserialize)] // rkyv serialization
#[derive(BorshSerialize, BorshDeserialize)] // borsh serialization
pub struct ResourceId([u8; 32]);

impl Deref for ResourceId {
    type Target = [u8; 32];
    fn deref(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for ResourceId {
    fn from(bytes: [u8; 32]) -> Self {
        ResourceId(bytes)
    }
}

impl<'a> From<&'a [u8; 32]> for &'a ResourceId {
    fn from(bytes: &'a [u8; 32]) -> Self {
        // SAFETY: ResourceId is repr(transparent) over [u8; 32], so the layout is identical.
        unsafe { &*(bytes as *const [u8; 32] as *const ResourceId) }
    }
}
