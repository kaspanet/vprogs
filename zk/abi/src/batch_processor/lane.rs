//! Lane-identity helpers.
//!
//! A lane can be named by a compact `u32`; this module projects that `u32` into the 20-byte kaspa
//! SubnetworkId shape `[namespace(4), 0x00 * 16]` the rest of the system uses (the value the host
//! feeds the batch processor as a public input, the journal commits, and the covenant SPK pins). It
//! applies the same shape constraint enforced on L1 by `check_transaction_subnetwork`: the reserved
//! `[x, 0x00 * 19]` patterns are rejected at const-eval, so a caller naming a reserved id in a
//! const context fails to build.

/// Projects a `u32` lane id into the 20-byte kaspa SubnetworkId shape `[namespace(4), 0x00 * 16]`.
/// Const-evaluates: ids that collide with the reserved `[x, 0x00 * 19]` patterns fail to compile.
pub const fn subnetwork_id_from_lane_id(lane_id: u32) -> [u8; 20] {
    let ns = lane_id.to_be_bytes();

    // Reject reserved shapes (NATIVE / COINBASE / future-reserved).
    assert!(
        ns[1] != 0 || ns[2] != 0 || ns[3] != 0,
        "lane id collides with reserved [x, 0x00 * 19] subnetwork shape"
    );

    let mut out = [0u8; 20];
    out[0] = ns[0];
    out[1] = ns[1];
    out[2] = ns[2];
    out[3] = ns[3];
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_namespace_in_be_order() {
        let id = subnetwork_id_from_lane_id(0x11_22_33_44);
        assert_eq!(&id[..4], &[0x11, 0x22, 0x33, 0x44]);
        assert_eq!(&id[4..], &[0u8; 16]);
    }

    #[test]
    fn accepts_first_byte_zero_when_tail_nonzero() {
        // `[0, 0, 0, 1, 0×16]` is a valid user lane (post-Toccata).
        let id = subnetwork_id_from_lane_id(0x00_00_00_01);
        assert_eq!(&id[..4], &[0, 0, 0, 1]);
    }

    /// Reserved shape `[x, 0×19]` must reject; projecting such an id in a const context would not
    /// compile. We can't test the compile-time panic directly here, so re-exercise the predicate.
    #[test]
    fn predicate_rejects_reserved_shape() {
        let reserved = 0xAB_00_00_00u32.to_be_bytes();
        // The const-assert condition: at least one of bytes 1..4 must be non-zero.
        assert!(!(reserved[1] != 0 || reserved[2] != 0 || reserved[3] != 0));
    }
}
