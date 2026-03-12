use crate::TREE_DEPTH;

/// Precomputed empty subtree hashes for each level.
///
/// `defaults[0]` is the empty leaf hash (`[0u8; 32]`).
/// `defaults[i]` = `blake3(defaults[i-1] || defaults[i-1])`.
/// `defaults[256]` is the root of a completely empty tree.
pub(crate) fn default_hashes() -> [[u8; 32]; TREE_DEPTH + 1] {
    let mut hashes = [[0u8; 32]; TREE_DEPTH + 1];
    for i in 1..=TREE_DEPTH {
        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(&hashes[i - 1]);
        buf[32..].copy_from_slice(&hashes[i - 1]);
        hashes[i] = *blake3::hash(&buf).as_bytes();
    }
    hashes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_hash_level_0_is_zeros() {
        let defaults = default_hashes();
        assert_eq!(defaults[0], [0u8; 32]);
    }

    #[test]
    fn default_hashes_are_consistent() {
        let defaults = default_hashes();
        for i in 1..=TREE_DEPTH {
            let mut buf = [0u8; 64];
            buf[..32].copy_from_slice(&defaults[i - 1]);
            buf[32..].copy_from_slice(&defaults[i - 1]);
            assert_eq!(defaults[i], *blake3::hash(&buf).as_bytes());
        }
    }
}
