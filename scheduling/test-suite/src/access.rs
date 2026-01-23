use vprogs_core_types::{AccessMetadata, AccessType};

/// A test access metadata specifying read or write access to a resource.
#[derive(Clone)]
pub enum Access {
    Read(usize),
    Write(usize),
}

impl AccessMetadata<usize> for Access {
    fn id(&self) -> usize {
        match self {
            Access::Read(id) | Access::Write(id) => *id,
        }
    }

    fn access_type(&self) -> AccessType {
        match self {
            Access::Read(_) => AccessType::Read,
            Access::Write(_) => AccessType::Write,
        }
    }
}
