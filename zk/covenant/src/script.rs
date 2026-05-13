//! P2SH redeem script for the settlement covenant.
//!
//! The script enforces four invariants on any settlement transaction spending the covenant UTXO:
//!
//! 1. **State continuation**: the single covenant output's SPK is a P2SH of this script rebuilt
//!    with the advanced `(new_state, new_lane_tip)` pair pushed as the prefix.
//! 2. **Journal binding**: the ZK proof's committed journal hashes to `sha256(prev_state ||
//!    prev_lane_tip || new_state || new_lane_tip || new_seq_commit || covenant_id || tx_image_id)`.
//!    `tx_image_id`, `control_id`, `hashfn`, and `image_id` are all hardcoded into the script body,
//!    so the covenant SPK pins the entire verifier identity - both the batch-proof verifier circuit
//!    and the inner transaction-processor guest the batch checked against.
//! 3. **Seq commitment freshness**: the journal's `new_seq_commit` equals
//!    `OpChainblockSeqCommit(block_prove_to)` pushed at spend time, which itself is derived by the
//!    guest from `new_lane_tip` - so the chain of UTXO-locked `lane_tip` values is load-bearing all
//!    the way through the seq-commit derivation, preventing rewinds.
//! 4. **Proof validity**: `OpZkPrecompile` verifies the risc0 succinct receipt against the
//!    configured image id.
//!
//! The MVP here omits the optional permission-tree exit path - the covenant always has
//! exactly one output.

use kaspa_hashes::Hash;
use kaspa_txscript::{
    opcodes::codes::{
        Op0, OpAdd, OpBlake2b, OpCat, OpChainblockSeqCommit, OpCovOutputCount, OpData32, OpDrop,
        OpDup, OpEqual, OpEqualVerify, OpFromAltStack, OpInputCovenantId, OpSHA256, OpSwap,
        OpToAltStack, OpTrue, OpTxInputIndex, OpTxInputScriptSigLen, OpTxInputScriptSigSubstr,
        OpTxOutputSpk, OpVerify, OpZkPrecompile,
    },
    script_builder::ScriptBuilder,
    zk_precompiles::tags::ZkTag,
};

/// Redeem script prefix size in bytes.
///
/// Layout (66 bytes total):
///
/// ```text
/// OpData32(1) | prev_lane_tip(32) | OpData32(1) | prev_state(32)
/// ```
pub const REDEEM_PREFIX_LEN: i64 = 66;

/// Verifier-identity constants hardcoded into the redeem script body. Constant across all
/// generations of a given covenant family; bundling them keeps the redeem-builder signature
/// compact and documents that these values are part of the covenant SPK identity rather than
/// per-spend witness data.
#[derive(Copy, Clone)]
pub struct RedeemPins<'a> {
    /// Batch processor guest image id (the proof verifier's image_id pushed before
    /// `OpZkPrecompile`).
    pub program_id: &'a [u8; 32],
    /// Transaction processor guest image id, cat-ed into the journal preimage so the inner
    /// proof verifier identity is constrained on-chain.
    pub tx_image_id: &'a [u8; 32],
    /// Risc0 succinct verifier control root (kaspa PR #957) pushed before `OpZkPrecompile`.
    pub control_id: &'a [u8; 32],
    /// Risc0 recursion hash function id (0 = blake2b, 1 = poseidon2, 2 = sha-256).
    pub hashfn: u8,
    /// Proof system tag selecting which precompile variant the script terminates in.
    pub zk_tag: ZkTag,
}

/// Builds the settlement redeem script for a covenant carrying the given
/// `(prev_state, prev_lane_tip)` pair. The fields of [`RedeemPins`] are all hardcoded into
/// the script body, so the covenant SPK pins the entire verifier identity. The script length
/// is stable across invocations with identical pins, so `redeem_script_len` can be computed
/// once and reused.
pub fn build_redeem_script(
    prev_state: &[u8; 32],
    prev_lane_tip: &Hash,
    redeem_script_len: i64,
    pins: &RedeemPins<'_>,
) -> Vec<u8> {
    let mut b = ScriptBuilder::new();

    // 66-byte data prefix (redeem-script-identifying bytes).
    b.add_data(prev_lane_tip.as_slice()).unwrap();
    b.add_data(prev_state).unwrap();

    stash_prev_values(&mut b);
    obtain_new_seq_commitment(&mut b);
    build_next_redeem_prefix(&mut b);
    extract_redeem_suffix_and_concat(&mut b, redeem_script_len);
    hash_redeem_to_spk(&mut b);
    verify_output_spk(&mut b);
    build_and_hash_journal(&mut b, pins.tx_image_id);

    match pins.zk_tag {
        ZkTag::R0Succinct => {
            verify_risc0_succinct(&mut b, pins.program_id, pins.control_id, pins.hashfn)
        }
        ZkTag::Groth16 => panic!("groth16 settlement not wired yet"),
    }

    verify_input_index_zero(&mut b);
    verify_covenant_single_output(&mut b);
    b.add_op(OpTrue).unwrap();

    // Domain suffix flagging "state verification" to distinguish the script from unrelated
    // covenants sharing the same ZK verifier pattern.
    b.add_op(Op0).unwrap();
    b.add_op(OpDrop).unwrap();

    b.drain()
}

/// Iteratively computes the redeem script length (which appears inside the script itself as an
/// offset), starting from a small initial guess.
pub fn redeem_script_len(prev_state: &[u8; 32], pins: &RedeemPins<'_>) -> i64 {
    let mut guess: i64 = 75;
    loop {
        let len = build_redeem_script(prev_state, &Hash::default(), guess, pins).len() as i64;
        if len == guess {
            return len;
        }
        guess = len;
    }
}

/// Builds a dev-mode settlement redeem script: identical structure to
/// [`build_redeem_script`] except the proof-side journal preimage and `OpZkPrecompile` call
/// are replaced with `OpEqualVerify` of a sig-script-supplied `claimed_seq_commit` against
/// `OpChainblockSeqCommit(block_prove_to)`.
///
/// Trade-off: drops `program_id`, `tx_image_id`, and the journal-hash binding. What remains
/// is still load-bearing on-chain:
/// - `prev_state` / `prev_lane_tip` are bound by this script's SPK.
/// - `new_state` / `new_lane_tip` are bound by the output-SPK continuation check.
/// - `covenant_id` is bound by [`CovenantsContext::from_tx`] at the consensus level.
/// - `new_seq_commit` is strictly equated to the chain's value for `block_prove_to`.
///
/// Use this only for tests that want to exercise the full chain pipeline (bootstrap, lane
/// carriers, mempool, block inclusion, acceptance data) without paying the cost of a real
/// CUDA-produced STARK seal.
pub fn build_dev_redeem_script(
    prev_state: &[u8; 32],
    prev_lane_tip: &Hash,
    redeem_script_len: i64,
) -> Vec<u8> {
    let mut b = ScriptBuilder::new();

    // Same 66-byte prefix as the production script (OpData32||prev_lane_tip||OpData32||prev_state).
    b.add_data(prev_lane_tip.as_slice()).unwrap();
    b.add_data(prev_state).unwrap();

    stash_prev_values(&mut b);
    obtain_new_seq_commitment(&mut b);
    verify_claimed_seq_commit(&mut b);
    build_next_redeem_prefix_no_stash(&mut b);
    extract_redeem_suffix_and_concat(&mut b, redeem_script_len);
    hash_redeem_to_spk(&mut b);
    verify_output_spk(&mut b);
    drop_stashed_prev_values(&mut b);
    verify_input_index_zero(&mut b);
    verify_covenant_single_output(&mut b);
    b.add_op(OpTrue).unwrap();

    b.drain()
}

/// Iteratively computes the dev redeem script length (the dev script also embeds its own
/// length as an offset, so the value is computed the same way as [`redeem_script_len`]).
pub fn dev_redeem_script_len(prev_state: &[u8; 32]) -> i64 {
    let mut guess: i64 = 75;
    loop {
        let len = build_dev_redeem_script(prev_state, &Hash::default(), guess).len() as i64;
        if len == guess {
            return len;
        }
        guess = len;
    }
}

/// Stack: `[..., new_lane_tip, new_state, block_prove_to, prev_lane_tip, prev_state]`
/// →      `[..., new_lane_tip, new_state, block_prove_to]`  alt: `[prev_state, prev_lane_tip]`
fn stash_prev_values(b: &mut ScriptBuilder) {
    b.add_op(OpToAltStack).unwrap();
    b.add_op(OpToAltStack).unwrap();
}

/// Replaces `block_prove_to` with the block's seq commitment.
///
/// Stack: `[..., new_lane_tip, new_state, block_prove_to]`
/// →      `[..., new_lane_tip, new_state, new_seq_commit]`
fn obtain_new_seq_commitment(b: &mut ScriptBuilder) {
    b.add_op(OpChainblockSeqCommit).unwrap();
}

/// Dev-mode only. Asserts that the sig-script-supplied `claimed_seq_commit` equals
/// `OpChainblockSeqCommit(block_prove_to)`. Consumes both seq-commit values and preserves
/// `new_state` / `new_lane_tip` on the stack for the prefix builder downstream.
///
/// Stack: `[..., claimed_seq_commit, new_lane_tip, new_state, chain_seq_commit]`
/// →      `[..., new_lane_tip, new_state]`
fn verify_claimed_seq_commit(b: &mut ScriptBuilder) {
    b.add_op(OpSwap).unwrap();
    // Stack: [..., claimed, new_lane_tip, chain_seq_commit, new_state]
    b.add_op(OpToAltStack).unwrap();
    // Stack: [..., claimed, new_lane_tip, chain_seq_commit]  alt:[..., new_state]
    b.add_op(OpSwap).unwrap();
    // Stack: [..., claimed, chain_seq_commit, new_lane_tip]
    b.add_op(OpToAltStack).unwrap();
    // Stack: [..., claimed, chain_seq_commit]  alt:[..., new_state, new_lane_tip]
    b.add_op(OpEqualVerify).unwrap();
    // Stack: [...]
    b.add_op(OpFromAltStack).unwrap();
    // Stack: [..., new_lane_tip]  alt:[..., new_state]
    b.add_op(OpFromAltStack).unwrap();
    // Stack: [..., new_lane_tip, new_state]
}

/// Dev-mode only. Same byte layout as [`build_next_redeem_prefix`] but skips the journal
/// stashes (no `new_seq_commit`, no dup/stash of `new_state` / `new_lane_tip`).
///
/// Stack: `[..., new_lane_tip, new_state]` → `[..., (OpData32||new_lane_tip||OpData32||new_state)]`
fn build_next_redeem_prefix_no_stash(b: &mut ScriptBuilder) {
    b.add_data(&[OpData32]).unwrap();
    b.add_op(OpSwap).unwrap();
    b.add_op(OpCat).unwrap();
    // Stack: [..., new_lane_tip, (OpData32||new_state)]
    b.add_op(OpSwap).unwrap();
    // Stack: [..., (OpData32||new_state), new_lane_tip]
    b.add_data(&[OpData32]).unwrap();
    b.add_op(OpSwap).unwrap();
    b.add_op(OpCat).unwrap();
    // Stack: [..., (OpData32||new_state), (OpData32||new_lane_tip)]
    b.add_op(OpSwap).unwrap();
    b.add_op(OpCat).unwrap();
    // Stack: [..., (OpData32||new_lane_tip||OpData32||new_state)] = 66B prefix
}

/// Dev-mode only. Pops and discards `prev_lane_tip` and `prev_state` from the alt stack
/// (production consumes them inside `build_and_hash_journal`; dev has no journal).
fn drop_stashed_prev_values(b: &mut ScriptBuilder) {
    b.add_op(OpFromAltStack).unwrap();
    b.add_op(OpDrop).unwrap();
    b.add_op(OpFromAltStack).unwrap();
    b.add_op(OpDrop).unwrap();
}

/// Constructs the 66-byte redeem-script prefix for the next covenant UTXO (embedding
/// `new_state` and `new_lane_tip` so the next spend can verify continuity) and stashes
/// `new_seq_commit`, `new_state`, `new_lane_tip` on the alt stack for later journal
/// construction.
///
/// Stack: `[..., new_lane_tip, new_state, new_seq_commit]` alt: `[prev_state, prev_lane_tip]`
/// →      `[..., prefix(66)]`
///        alt: `[prev_state, prev_lane_tip, new_seq_commit, new_state, new_lane_tip]`
fn build_next_redeem_prefix(b: &mut ScriptBuilder) {
    // Stash new_seq_commit (consumed by journal, not needed in prefix).
    b.add_op(OpToAltStack).unwrap();
    // Stack: [..., new_lane_tip, new_state]

    // Dup + stash new_state, then build (OpData32 || new_state).
    b.add_op(OpDup).unwrap();
    b.add_op(OpToAltStack).unwrap();
    b.add_data(&[OpData32]).unwrap();
    b.add_op(OpSwap).unwrap();
    b.add_op(OpCat).unwrap();
    // Stack: [..., new_lane_tip, (OpData32||new_state)]

    b.add_op(OpSwap).unwrap();
    // Stack: [..., (OpData32||new_state), new_lane_tip]

    // Dup + stash new_lane_tip, then build (OpData32 || new_lane_tip).
    b.add_op(OpDup).unwrap();
    b.add_op(OpToAltStack).unwrap();
    b.add_data(&[OpData32]).unwrap();
    b.add_op(OpSwap).unwrap();
    b.add_op(OpCat).unwrap();
    // Stack: [..., (OpData32||new_state), (OpData32||new_lane_tip)]

    b.add_op(OpSwap).unwrap();
    b.add_op(OpCat).unwrap();
    // Stack: [..., (OpData32||new_lane_tip||OpData32||new_state)] = 66B prefix
}

/// Appends the current redeem script's suffix (everything after the first
/// [`REDEEM_PREFIX_LEN`] bytes) to the stacked prefix, yielding the full next redeem script.
fn extract_redeem_suffix_and_concat(b: &mut ScriptBuilder, redeem_script_len: i64) {
    b.add_op(OpTxInputIndex).unwrap();
    b.add_op(OpTxInputIndex).unwrap();
    b.add_op(OpTxInputScriptSigLen).unwrap();
    b.add_i64(-redeem_script_len + REDEEM_PREFIX_LEN).unwrap();
    b.add_op(OpAdd).unwrap();
    b.add_op(OpTxInputIndex).unwrap();
    b.add_op(OpTxInputScriptSigLen).unwrap();
    b.add_op(OpTxInputScriptSigSubstr).unwrap();
    b.add_op(OpCat).unwrap();
}

/// Hashes the redeem script and wraps it in a version-tagged P2SH SPK.
///
/// SPK layout: `version(2) | OpBlake2b(1) | OpData32(1) | hash(32) | OpEqual(1)` = 37 bytes.
fn hash_redeem_to_spk(b: &mut ScriptBuilder) {
    b.add_op(OpBlake2b).unwrap();
    let mut header = [0u8; 4];
    header[0..2].copy_from_slice(
        &kaspa_consensus_core::constants::MAX_SCRIPT_PUBLIC_KEY_VERSION.to_le_bytes(),
    );
    header[2] = OpBlake2b;
    header[3] = OpData32;
    b.add_data(&header).unwrap();
    b.add_op(OpSwap).unwrap();
    b.add_op(OpCat).unwrap();
    b.add_data(&[OpEqual]).unwrap();
    b.add_op(OpCat).unwrap();
}

/// Verifies the output at `OpTxInputIndex` matches the constructed SPK.
fn verify_output_spk(b: &mut ScriptBuilder) {
    b.add_op(OpTxInputIndex).unwrap();
    b.add_op(OpTxOutputSpk).unwrap();
    b.add_op(OpEqualVerify).unwrap();
}

/// Consumes the alt-stack stash
/// `(prev_state, prev_lane_tip, new_seq_commit, new_state, new_lane_tip)`, assembles the
/// 160-byte state preimage, appends the input's covenant id (→ 192B) and the script-embedded
/// `tx_image_id` (→ 224B), and SHA-256s the result.
///
/// Alt layout going in (top→bottom):
/// `[new_lane_tip, new_state, new_seq_commit, prev_lane_tip, prev_state]`
///
/// Output: `[..., journal_hash]`
fn build_and_hash_journal(b: &mut ScriptBuilder, tx_image_id: &[u8; 32]) {
    // Pop the three "new" values.
    b.add_op(OpFromAltStack).unwrap(); // new_lane_tip
    b.add_op(OpFromAltStack).unwrap(); // new_state
    b.add_op(OpFromAltStack).unwrap(); // new_seq_commit
    // Stack: [..., new_lane_tip, new_state, new_seq_commit]

    // Build (new_state || new_lane_tip || new_seq_commit) = 96 bytes.
    b.add_op(OpSwap).unwrap();
    // Stack: [..., new_lane_tip, new_seq_commit, new_state]
    b.add_op(OpToAltStack).unwrap();
    // Stack: [..., new_lane_tip, new_seq_commit]  alt:[..., new_state]
    b.add_op(OpSwap).unwrap();
    // Stack: [..., new_seq_commit, new_lane_tip]
    b.add_op(OpFromAltStack).unwrap();
    // Stack: [..., new_seq_commit, new_lane_tip, new_state]
    b.add_op(OpSwap).unwrap();
    // Stack: [..., new_seq_commit, new_state, new_lane_tip]
    b.add_op(OpCat).unwrap();
    // Stack: [..., new_seq_commit, (new_state || new_lane_tip)]
    b.add_op(OpSwap).unwrap();
    b.add_op(OpCat).unwrap();
    // Stack: [..., (new_state || new_lane_tip || new_seq_commit)] = 96B

    // Pop prev values and build (prev_state || prev_lane_tip) = 64 bytes.
    b.add_op(OpFromAltStack).unwrap(); // prev_lane_tip
    b.add_op(OpFromAltStack).unwrap(); // prev_state
    b.add_op(OpSwap).unwrap();
    b.add_op(OpCat).unwrap();
    // Stack: [..., 96B_new, (prev_state || prev_lane_tip)] = 64B

    b.add_op(OpSwap).unwrap();
    b.add_op(OpCat).unwrap();
    // Stack: [..., (prev_state || prev_lane_tip || new_state || new_lane_tip || new_seq_commit)] =
    // 160B

    // Append covenant_id → 192B preimage.
    b.add_op(OpTxInputIndex).unwrap();
    b.add_op(OpInputCovenantId).unwrap();
    b.add_op(OpCat).unwrap();

    // Append script-embedded tx_image_id → 224B preimage.
    b.add_data(tx_image_id).unwrap();
    b.add_op(OpCat).unwrap();

    b.add_op(OpSHA256).unwrap();
}

/// Asserts that the spending input is at index 0 (covenant always lives at input 0).
fn verify_input_index_zero(b: &mut ScriptBuilder) {
    b.add_op(OpTxInputIndex).unwrap();
    b.add_i64(0).unwrap();
    b.add_op(OpEqualVerify).unwrap();
}

/// Asserts that the covenant creates exactly one output (no permission-tree exit).
fn verify_covenant_single_output(b: &mut ScriptBuilder) {
    b.add_op(OpTxInputIndex).unwrap();
    b.add_op(OpInputCovenantId).unwrap();
    b.add_op(OpCovOutputCount).unwrap();
    b.add_i64(1).unwrap();
    b.add_op(OpEqualVerify).unwrap();
}

/// Emits the risc0 succinct (STARK) verification.
///
/// Stack arriving here (bot→top):
/// `[..., claim, control_index, control_digests, seal, journal_hash]`
///
/// Pushes `image_id`, `control_id`, `hashfn` from the script body in the natural pop order
/// of R0Succinct (top→bottom:
/// `hashfn, control_id, image_id, journal, seal, control_digests, control_index, claim`),
/// then the `ZkTag` byte and `OpZkPrecompile`.
fn verify_risc0_succinct(
    b: &mut ScriptBuilder,
    image_id: &[u8; 32],
    control_id: &[u8; 32],
    hashfn: u8,
) {
    b.add_data(image_id).unwrap();
    b.add_data(control_id).unwrap();
    b.add_data(&[hashfn]).unwrap();
    b.add_data(&[ZkTag::R0Succinct as u8]).unwrap();
    b.add_op(OpZkPrecompile).unwrap();
    b.add_op(OpVerify).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_pins<'a>(
        program_id: &'a [u8; 32],
        tx_image_id: &'a [u8; 32],
        control_id: &'a [u8; 32],
    ) -> RedeemPins<'a> {
        RedeemPins { program_id, tx_image_id, control_id, hashfn: 1, zk_tag: ZkTag::R0Succinct }
    }

    #[test]
    fn redeem_script_length_converges() {
        let state = [0u8; 32];
        let pins = test_pins(&[1u8; 32], &[2u8; 32], &[3u8; 32]);
        let len = redeem_script_len(&state, &pins);
        let built = build_redeem_script(&state, &Hash::default(), len, &pins);
        assert_eq!(built.len() as i64, len, "self-referential length must match");
    }

    #[test]
    fn redeem_script_length_stable_across_values() {
        let pins = test_pins(&[1u8; 32], &[2u8; 32], &[3u8; 32]);
        let a = redeem_script_len(&[0u8; 32], &pins);
        let b = redeem_script_len(&[0xffu8; 32], &pins);
        assert_eq!(a, b, "length must not depend on prev state");
    }

    #[test]
    fn dev_redeem_script_length_converges() {
        let state = [0u8; 32];
        let len = dev_redeem_script_len(&state);
        let built = build_dev_redeem_script(&state, &Hash::default(), len);
        assert_eq!(built.len() as i64, len, "self-referential length must match");
    }

    #[test]
    fn dev_redeem_script_distinct_from_prod() {
        let state = [0u8; 32];
        let pins = test_pins(&[1u8; 32], &[2u8; 32], &[3u8; 32]);
        let prod_len = redeem_script_len(&state, &pins);
        let prod = build_redeem_script(&state, &Hash::default(), prod_len, &pins);
        let dev_len = dev_redeem_script_len(&state);
        let dev = build_dev_redeem_script(&state, &Hash::default(), dev_len);
        assert_ne!(prod, dev, "dev and prod scripts must differ");
        assert!(dev.len() < prod.len(), "dev script should be shorter (no journal/zk verify)");
    }
}
