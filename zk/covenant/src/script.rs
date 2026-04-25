//! P2SH redeem script for the settlement covenant.
//!
//! The script enforces four invariants on any settlement transaction spending the covenant UTXO:
//!
//! 1. **State continuation**: the single covenant output's SPK is a P2SH of this script rebuilt
//!    with the advanced `(new_state, new_lane_tip)` pair pushed as the prefix.
//! 2. **Journal binding**: the ZK proof's committed journal hashes to `sha256(prev_state ||
//!    prev_lane_tip || new_state || new_lane_tip || new_seq_commit || covenant_id)`.
//! 3. **Seq commitment freshness**: the journal's `new_seq_commit` equals
//!    `OpChainblockSeqCommit(block_prove_to)` pushed at spend time, which itself is derived by the
//!    guest from `new_lane_tip` via kip21 — so the chain of UTXO-locked `lane_tip` values is
//!    load-bearing all the way through the seq-commit derivation, preventing rewinds.
//! 4. **Proof validity**: `OpZkPrecompile` verifies the risc0 succinct receipt against the
//!    configured image id.
//!
//! Structure mirrors biryukovmaxim's `zk-covenant-rollup` (kip21 variant). The MVP here omits
//! the optional permission-tree exit path - the covenant always has exactly one output.

use kaspa_txscript::{
    opcodes::codes::{
        Op0, Op2Swap, OpAdd, OpBlake2b, OpCat, OpChainblockSeqCommit, OpCovOutputCount, OpData32,
        OpDrop, OpDup, OpEqual, OpEqualVerify, OpFromAltStack, OpInputCovenantId, OpSHA256, OpSwap,
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

/// Builds the settlement redeem script for a covenant carrying the given
/// `(prev_state, prev_lane_tip)` pair. `tx_image_id` is hardcoded into the script body and
/// stashed for the journal preimage so the on-chain proof binds to a specific
/// transaction-processor identity. The script length is stable across invocations with
/// identical `program_id`, `tx_image_id`, and `zk_tag`, so `redeem_script_len` can be computed
/// once and reused.
pub fn build_redeem_script(
    prev_state: &[u8; 32],
    prev_lane_tip: &[u8; 32],
    redeem_script_len: i64,
    program_id: &[u8; 32],
    tx_image_id: &[u8; 32],
    zk_tag: ZkTag,
) -> Vec<u8> {
    let mut b = ScriptBuilder::new();

    // 66-byte data prefix (redeem-script-identifying bytes).
    b.add_data(prev_lane_tip).unwrap();
    b.add_data(prev_state).unwrap();

    // Hardcoded inner-guest identity: the batch guest's `env::verify` call uses an input-supplied
    // image id, so without a script-level binding the host could swap in a backdoored
    // tx-processor. Stashing it to alt before the prev-value pushes places it at the bottom of
    // the alt stack so `build_and_hash_journal` can pop it last and cat it into the preimage.
    b.add_data(tx_image_id).unwrap();
    b.add_op(OpToAltStack).unwrap();

    stash_prev_values(&mut b);
    obtain_new_seq_commitment(&mut b);
    build_next_redeem_prefix(&mut b);
    extract_redeem_suffix_and_concat(&mut b, redeem_script_len);
    hash_redeem_to_spk(&mut b);
    verify_output_spk(&mut b);
    build_and_hash_journal(&mut b);

    b.add_data(program_id).unwrap();

    match zk_tag {
        ZkTag::R0Succinct => verify_risc0_succinct(&mut b),
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
pub fn redeem_script_len(
    prev_state: &[u8; 32],
    program_id: &[u8; 32],
    tx_image_id: &[u8; 32],
    zk_tag: ZkTag,
) -> i64 {
    let mut guess: i64 = 75;
    loop {
        let len =
            build_redeem_script(prev_state, prev_state, guess, program_id, tx_image_id, zk_tag)
                .len() as i64;
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
/// `(tx_image_id, prev_state, prev_lane_tip, new_seq_commit, new_state, new_lane_tip)`,
/// assembles the 160-byte state preimage, appends the input's covenant id (→ 192B) and the
/// stashed `tx_image_id` (→ 224B), and SHA-256s the result.
///
/// Alt layout going in (top→bottom):
/// `[new_lane_tip, new_state, new_seq_commit, prev_lane_tip, prev_state, tx_image_id]`
///
/// Output: `[..., journal_hash]`
fn build_and_hash_journal(b: &mut ScriptBuilder) {
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

    // Append tx_image_id (stashed at the bottom of the alt stack at script start) → 224B.
    b.add_op(OpFromAltStack).unwrap();
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
/// `[..., claim, control_index, control_digests, seal, control_id, hashfn, journal_hash, image_id]`
///
/// `Op2Swap` reshuffles the top four items from
/// `[control_id, hashfn, journal_hash, image_id]` to
/// `[journal_hash, image_id, control_id, hashfn]` so the final layout matches
/// R0Succinct's pop order (top→bottom:
/// `hashfn, control_id, image_id, journal, seal, control_digests, control_index, claim`).
/// Kaspa PR #957 introduced the upstream `control_id` check — the sig_script supplies it
/// alongside `hashfn` right after `seal`, and this swap moves them past the redeem-derived
/// `journal_hash, image_id`.
fn verify_risc0_succinct(b: &mut ScriptBuilder) {
    b.add_op(Op2Swap).unwrap();
    b.add_data(&[ZkTag::R0Succinct as u8]).unwrap();
    b.add_op(OpZkPrecompile).unwrap();
    b.add_op(OpVerify).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redeem_script_length_converges() {
        let state = [0u8; 32];
        let program_id = [1u8; 32];
        let tx_image_id = [2u8; 32];
        let len = redeem_script_len(&state, &program_id, &tx_image_id, ZkTag::R0Succinct);
        let built =
            build_redeem_script(&state, &state, len, &program_id, &tx_image_id, ZkTag::R0Succinct);
        assert_eq!(built.len() as i64, len, "self-referential length must match");
    }

    #[test]
    fn redeem_script_length_stable_across_values() {
        let program_id = [1u8; 32];
        let tx_image_id = [2u8; 32];
        let a = redeem_script_len(&[0u8; 32], &program_id, &tx_image_id, ZkTag::R0Succinct);
        let b = redeem_script_len(&[0xffu8; 32], &program_id, &tx_image_id, ZkTag::R0Succinct);
        assert_eq!(a, b, "length must not depend on prev state");
    }
}
