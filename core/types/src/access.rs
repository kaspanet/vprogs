use borsh::{BorshDeserialize, BorshSerialize};

use crate::{AccessType, ResourceId};

#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct Access {
    pub id: ResourceId,
    pub access_type: AccessType,
}

impl Access {
    pub fn read(id: impl Into<ResourceId>) -> Self {
        Self { id: id.into(), access_type: AccessType::Read }
    }

    pub fn write(id: impl Into<ResourceId>) -> Self {
        Self { id: id.into(), access_type: AccessType::Write }
    }
}
