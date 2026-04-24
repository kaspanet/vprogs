//! P2SH redeem script for the settlement covenant.
//!
//! The script enforces four invariants on any settlement transaction spending the covenant UTXO:
//!
//! 1. **State continuation**: the single covenant output's SPK is a P2SH of this script rebuilt
//!    with the new `(state, seq)` pair pushed as the prefix.
//! 2. **Journal binding**: the ZK proof's committed journal hashes to `sha256(prev_state ||
//!    prev_seq || new_state || new_seq || covenant_id)`.
//! 3. **Seq commitment freshness**: the journal's `new_seq` equals
//!    `OpChainblockSeqCommit(block_prove_to)` pushed at spend time.
//! 4. **Proof validity**: `OpZkPrecompile` verifies the risc0 succinct receipt against the
//!    configured image id.
//!
//! Structure mirrors `zk-covenant-rollup` (biryukovmaxim). The MVP here omits the optional
//! permission-tree exit path - the covenant always has exactly one output.

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
/// OpData32(1) | prev_seq(32) | OpData32(1) | prev_state(32)
/// ```
pub const REDEEM_PREFIX_LEN: i64 = 66;

/// Builds the settlement redeem script for a covenant carrying the given `(prev_state, prev_seq)`
/// pair. The script length is stable across invocations with identical `program_id` and
/// `zk_tag`, so `redeem_script_len` can be computed once and reused.
pub fn build_redeem_script(
    prev_state: &[u8; 32],
    prev_seq: &[u8; 32],
    redeem_script_len: i64,
    program_id: &[u8; 32],
    zk_tag: ZkTag,
) -> Vec<u8> {
    let mut b = ScriptBuilder::new();

    b.add_data(prev_seq).unwrap();
    b.add_data(prev_state).unwrap();

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
pub fn redeem_script_len(prev_state: &[u8; 32], program_id: &[u8; 32], zk_tag: ZkTag) -> i64 {
    let mut guess: i64 = 75;
    loop {
        let len =
            build_redeem_script(prev_state, prev_state, guess, program_id, zk_tag).len() as i64;
        if len == guess {
            return len;
        }
        guess = len;
    }
}

/// Stack: `[..., block_prove_to, new_state, prev_seq, prev_state]`
/// →      `[..., block_prove_to, new_state]`  alt: `[prev_state, prev_seq]`
fn stash_prev_values(b: &mut ScriptBuilder) {
    b.add_op(OpToAltStack).unwrap();
    b.add_op(OpToAltStack).unwrap();
}

/// Replaces `block_prove_to` with the block's seq commitment.
///
/// Stack: `[..., block_prove_to, new_state]` → `[..., new_state, new_seq]`
fn obtain_new_seq_commitment(b: &mut ScriptBuilder) {
    b.add_op(OpSwap).unwrap();
    b.add_op(OpChainblockSeqCommit).unwrap();
}

/// Constructs the 66-byte redeem-script prefix for the next covenant UTXO and duplicates
/// `new_state`/`new_seq` onto the alt stack for later journal construction.
///
/// Stack: `[..., new_state, new_seq]` alt: `[prev_state, prev_seq]`
/// →      `[..., prefix(66)]`         alt: `[prev_state, prev_seq, new_state, new_seq]`
fn build_next_redeem_prefix(b: &mut ScriptBuilder) {
    b.add_op(OpSwap).unwrap();
    b.add_op(OpDup).unwrap();
    b.add_op(OpToAltStack).unwrap();
    b.add_data(&[OpData32]).unwrap();
    b.add_op(OpSwap).unwrap();
    b.add_op(OpCat).unwrap();
    b.add_op(OpSwap).unwrap();
    b.add_op(OpDup).unwrap();
    b.add_op(OpToAltStack).unwrap();
    b.add_data(&[OpData32]).unwrap();
    b.add_op(OpSwap).unwrap();
    b.add_op(OpCat).unwrap();
    b.add_op(OpSwap).unwrap();
    b.add_op(OpCat).unwrap();
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

/// Consumes the alt-stack-stashed `(prev_state, prev_seq, new_state, new_seq)`, concatenates them
/// into the 128-byte prev||new preimage, appends the input's covenant id, and SHA-256s the
/// result.
///
/// Stack: alt=`[new_seq, new_state, prev_seq, prev_state]` (top-to-bottom)
/// →      `[..., journal_hash]`
fn build_and_hash_journal(b: &mut ScriptBuilder) {
    b.add_op(OpFromAltStack).unwrap(); // new_seq
    b.add_op(OpFromAltStack).unwrap(); // new_state
    b.add_op(OpSwap).unwrap();
    b.add_op(OpCat).unwrap(); // new_state || new_seq
    b.add_op(OpFromAltStack).unwrap(); // prev_seq
    b.add_op(OpFromAltStack).unwrap(); // prev_state
    b.add_op(OpSwap).unwrap();
    b.add_op(OpCat).unwrap(); // prev_state || prev_seq
    b.add_op(OpSwap).unwrap();
    b.add_op(OpCat).unwrap(); // prev || new (128B)

    b.add_op(OpTxInputIndex).unwrap();
    b.add_op(OpInputCovenantId).unwrap();
    b.add_op(OpCat).unwrap(); // 160B preimage

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

/// Emits the risc0 succinct (STARK) verification ops.
///
/// Stack: `[seal, claim, hashfn, control_index, control_digests, journal_hash, image_id]`
/// →      `[]`
fn verify_risc0_succinct(b: &mut ScriptBuilder) {
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
        let len = redeem_script_len(&state, &program_id, ZkTag::R0Succinct);
        let built = build_redeem_script(&state, &state, len, &program_id, ZkTag::R0Succinct);
        assert_eq!(built.len() as i64, len, "self-referential length must match");
    }

    #[test]
    fn redeem_script_length_stable_across_values() {
        let program_id = [1u8; 32];
        let a = redeem_script_len(&[0u8; 32], &program_id, ZkTag::R0Succinct);
        let b = redeem_script_len(&[0xffu8; 32], &program_id, ZkTag::R0Succinct);
        assert_eq!(a, b, "length must not depend on prev state");
    }
}
