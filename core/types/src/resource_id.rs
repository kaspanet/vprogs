use core::ops::Deref;

use borsh::{BorshDeserialize, BorshSerialize};
use rkyv::{Archive, Deserialize, Serialize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[derive(Archive, Serialize, Deserialize)] // rkyv serialization
#[derive(BorshSerialize, BorshDeserialize)] // borsh serialization
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Unaligned)] // zerocopy
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
