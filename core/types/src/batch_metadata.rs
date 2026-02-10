use std::fmt::Debug;

use borsh::{BorshDeserialize, BorshSerialize};

/// Opaque metadata attached to each scheduler batch, supporting serialization.
///
/// Implementors derive `BorshSerialize` and `BorshDeserialize`; the trait is
/// implemented automatically via a blanket impl (same pattern as [`super::ResourceId`]).
pub trait BatchMetadata:
    BorshSerialize + BorshDeserialize + Clone + Debug + Default + Send + Sync + 'static
{
}

impl<T> BatchMetadata for T where
    T: BorshSerialize + BorshDeserialize + Clone + Debug + Default + Send + Sync + 'static
{
}
