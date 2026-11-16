use kaspa_hashes::{Hash, HasherBase, SeqCommitMergesetContext};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned, little_endian::U64};

/// Mergeset context of the chain block a transaction executes against: the preimage the per-batch
/// context hash commits to, and the VM's source of on-chain randomness.
///
/// Every field is an unaligned little-endian `u64`, so the struct doubles as its own wire encoding:
/// written with [`IntoBytes::as_bytes`], read zero-copy via `Reader::array_as`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout, Unaligned)]
pub struct MergesetContext {
    /// Previous block's header timestamp in milliseconds.
    pub timestamp: U64,
    /// DAA score at the block's position.
    pub daa_score: U64,
    /// DAG blue score at the block's position.
    pub blue_score: U64,
}

impl MergesetContext {
    /// The all-zero context; harnesses that pin no chain context use it as a placeholder.
    pub const ZERO: Self =
        Self { timestamp: U64::new(0), daa_score: U64::new(0), blue_score: U64::new(0) };

    /// Domain-separated digest of this context, byte-identical to the L1-side
    /// `mergeset_context_hash` over the same fields.
    pub fn hash(&self) -> Hash {
        let mut hasher = SeqCommitMergesetContext::new();
        hasher.update(self.as_bytes());
        hasher.finalize()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The digest must stay byte-identical to the L1-side derivation: journals committed with
    /// [`MergesetContext::hash`] are checked against a hash recomputed from chain pins.
    #[test]
    fn hash_matches_l1_derivation() {
        let ctx = MergesetContext {
            timestamp: U64::new(1_700_000_000_123),
            daa_score: U64::new(100_000),
            blue_score: U64::new(50_000),
        };
        assert_eq!(
            ctx.hash(),
            kaspa_seq_commit::hashing::mergeset_context_hash(
                &kaspa_seq_commit::types::MergesetContext {
                    timestamp: ctx.timestamp.get(),
                    daa_score: ctx.daa_score.get(),
                    blue_score: ctx.blue_score.get(),
                }
            )
        );
    }
}
