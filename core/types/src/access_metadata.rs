use borsh::{BorshDeserialize, BorshSerialize};
use rkyv::{Archive, Deserialize, Serialize};

use crate::{AccessType, ResourceId};

#[derive(Clone, Copy, Debug)]
#[derive(Archive, Serialize, Deserialize)] // rkyv serialization
#[derive(BorshSerialize, BorshDeserialize)] // borsh serialization
pub struct AccessMetadata {
    pub id: ResourceId,
    pub access_type: AccessType,
}

impl AccessMetadata {
    pub fn read(id: impl Into<ResourceId>) -> Self {
        Self { id: id.into(), access_type: AccessType::Read }
    }

    pub fn write(id: impl Into<ResourceId>) -> Self {
        Self { id: id.into(), access_type: AccessType::Write }
    }
}
