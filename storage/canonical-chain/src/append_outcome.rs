/// Outcome of [`CanonicalChainManager::append`](crate::CanonicalChainManager::append).
#[derive(Debug, Clone, Copy)]
pub struct AppendOutcome {
    /// Canonical id of the appended block.
    pub id: u64,
    /// `true` if a fresh id was allocated; `false` if a returning block reused its existing id.
    pub is_new: bool,
}
