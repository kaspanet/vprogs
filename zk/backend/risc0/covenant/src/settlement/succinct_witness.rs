/// Serialized pieces of a risc0 succinct receipt that the covenant script consumes as the ZK
/// witness. These correspond to `SuccinctReceipt` fields the host pushes onto the script
/// stack. The verifier-identity constants (`control_id`, `hashfn`, `image_id`) are NOT here;
/// they're hardcoded into the redeem script body and supplied to `OpZkPrecompile` from there.
pub struct SuccinctWitness<'a> {
    /// STARK seal serialized as little-endian bytes of each `u32` word.
    pub seal: &'a [u8],
    /// 32-byte receipt claim digest.
    pub claim: &'a [u8; 32],
    /// Control-inclusion-proof leaf index (little-endian u32).
    pub control_index: u32,
    /// Concatenated 32-byte control-inclusion-proof path digests.
    pub control_digests: &'a [u8],
    /// Bundle deposit-address commitment (`StateTransition::deposit_spk_hash`), or `[0; 32]` for a
    /// no-deposit bundle. Witnessed (not pinned), bound by the redeem script's
    /// `verify_and_append_deposit_hash`.
    pub deposit_spk_hash: &'a [u8; 32],
}
