use std::{fmt::Debug, hash::Hash};

use borsh::{BorshDeserialize, BorshSerialize};

pub trait ResourceId:
    BorshSerialize + BorshDeserialize + Debug + Default + Eq + Hash + Clone + Send + Sync + 'static
{
}

impl<T> ResourceId for T where
    T: BorshSerialize
        + BorshDeserialize
        + Debug
        + Default
        + Eq
        + Hash
        + Clone
        + Send
        + Sync
        + 'static
{
}
