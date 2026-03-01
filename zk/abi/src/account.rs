use alloc::vec::Vec;

use rkyv::{Archive, Serialize};
use vprogs_core_types::ResourceId;

/// A snapshot of a single account's state at execution time.
#[derive(Clone, Debug, Archive, Serialize)]
pub struct Account {
    pub resource_id: ResourceId,
    pub data: Vec<u8>,
}
