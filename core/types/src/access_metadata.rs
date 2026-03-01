use borsh::{BorshDeserialize, BorshSerialize};
use rkyv::{Archive, Deserialize, Serialize};

use crate::{AccessType, ResourceId};

#[derive(Clone, Copy, Debug)]
#[derive(Archive, Serialize, Deserialize)] // rkyv serialization
#[derive(BorshSerialize, BorshDeserialize)] // borsh serialization
pub struct AccessMetadata {
    pub resource_id: ResourceId,
    pub access_type: AccessType,
}

impl AccessMetadata {
    pub fn read(resource_id: ResourceId) -> Self {
        Self { resource_id, access_type: AccessType::Read }
    }

    pub fn write(resource_id: ResourceId) -> Self {
        Self { resource_id, access_type: AccessType::Write }
    }
}
