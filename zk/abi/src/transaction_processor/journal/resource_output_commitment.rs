/// A single resource's output commitment: `Some(hash)` if changed, `None` if unchanged.
pub type ResourceOutputCommitment<'a> = Option<&'a [u8; 32]>;
