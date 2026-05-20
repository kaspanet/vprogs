//! Regression test for the lane-key-not-pinned soundness hole.
//!
//! Before the fix the settlement covenant pinned `prev_state` and `prev_lane_tip` but not the lane
//! the proof was supposed to represent: on the `lane_expired = true` branch (entered by every
//! first-after-bootstrap settlement and every dormant-lane revival) a prover could supply any other
//! lane's `lane_key`, replay that lane's tx journals, and produce a `new_seq_commit` that matched
//! L1's `accepted_id_merkle_root` verbatim. The covenant SPK would accept the settlement and be
//! silently re-pinned to a foreign lane.
//!
//! The fix pins the lane identity in two places: the batch-processor journal carries the lane's
//! 20-byte SubnetworkId, and the redeem-script prefix pins the same 20 bytes. Either pin alone
//! closes the hole; both together provide redundant defense. This test asserts the structural fix
//! is in place by checking the journal layout and the redeem-script prefix length.

use vprogs_zk_abi::batch_processor::{JOURNAL_SIZE, StateTransition};
use vprogs_zk_backend_risc0_covenant::REDEEM_PREFIX_LEN;

#[test]
fn journal_carries_a_lane_pin() {
    const _: () = assert!(JOURNAL_SIZE >= 8 * 32 + 20);
    assert_eq!(JOURNAL_SIZE, core::mem::size_of::<StateTransition>());
}

#[test]
fn redeem_prefix_carries_a_lane_pin() {
    // Production prefix layout: OpData20|subnetwork_id(20) | OpData32|prev_lane_tip(32) |
    // OpData32|prev_state(32) = 87 bytes.
    const _: () = assert!(REDEEM_PREFIX_LEN >= 87);
}
