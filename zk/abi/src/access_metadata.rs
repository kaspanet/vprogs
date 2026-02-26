use vprogs_core_types::AccessType;

use crate::ResourceId;

/// Per-access metadata for the ZK VM.
#[derive(Clone, Debug)]
pub struct AccessMetadata {
    pub id: ResourceId,
    pub access_type: AccessType,
}

impl vprogs_core_types::AccessMetadata<ResourceId> for AccessMetadata {
    fn id(&self) -> ResourceId {
        self.id.clone()
    }

    fn access_type(&self) -> AccessType {
        self.access_type
    }
}
