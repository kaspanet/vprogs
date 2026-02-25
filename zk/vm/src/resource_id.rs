use borsh::{BorshDeserialize, BorshSerialize};

/// Resource identifier for the ZK VM — an opaque byte vector.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq, BorshSerialize, BorshDeserialize)]
pub struct ResourceId(pub Vec<u8>);
