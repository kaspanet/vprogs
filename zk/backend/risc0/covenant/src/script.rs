//! P2SH redeem script for the settlement covenant.
//!
//! The script enforces five invariants on any settlement transaction spending the covenant UTXO:
//!
//! 1. **State continuation**: the index-0 covenant output's SPK is a P2SH of this script rebuilt
//!    with the advanced `(new_state, new_lane_tip)` pair pushed as the prefix.
//! 2. **Journal binding**: the ZK proof's committed journal hashes to `sha256(prev_state ||
//!    prev_lane_tip || new_state || new_lane_tip || new_seq_commit || covenant_id || tx_image_id ||
//!    permission_spk_hash)` - 256 bytes total. `tx_image_id`, `control_id`, `hashfn`, and
//!    `image_id` are all hardcoded into the script body, so the covenant SPK pins the entire
//!    verifier identity - both the batch-proof verifier circuit and the inner transaction-processor
//!    guest the batch checked against. The final 32 bytes (`permission_spk_hash`) are either the
//!    batch's L2→L1 exit commitment or `[0; 32]` for a no-exit batch (matching how the guest
//!    encodes the field).
//! 3. **Seq commitment freshness**: the journal's `new_seq_commit` equals
//!    `OpChainblockSeqCommit(block_prove_to)` pushed at spend time, which itself is derived by the
//!    guest from `new_lane_tip` - so the chain of UTXO-locked `lane_tip` values is load-bearing all
//!    the way through the seq-commit derivation, preventing rewinds.
//! 4. **Proof validity**: `OpZkPrecompile` verifies the risc0 succinct receipt against the
//!    configured image id.
//! 5. **Permission-output binding**: settlements have exactly one or two covenant-bound outputs,
//!    with `journal[permission_spk_hash] == [0; 32]` ⇔ count == 1 (no permission output) and
//!    `journal[permission_spk_hash] != [0; 32]` ⇔ count == 2 (permission output present). Output 0
//!    is always the continuation (invariant 1). In the count == 2 case the second covenant-bound
//!    output's SPK is `permission_spk(permission_spk_hash)` and its value equals
//!    `pins.permission_output_value`. The first covenant-bound output is pinned to tx-output index
//!    0 (and the second to index 1 in the count==2 case) via `OpCovOutputIdx`, so the SPK check at
//!    output 0 and the permission-output check at output 1 are guaranteed to land on the
//!    covenant-bound outputs rather than non-bound siblings the host could have interleaved. The
//!    count==2 branch also explicitly rejects an extracted permission hash of `[0; 32]`; that
//!    hash is the guest's no-exits sentinel and would otherwise lock the operator's permission
//!    dust to an unspendable script. All these rules are enforced inside
//!    `verify_outputs_and_append_perm_hash`.

use kaspa_hashes::Hash;
use kaspa_txscript::{
    opcodes::codes::{
        Op0, OpAdd, OpBlake2b, OpCat, OpChainblockSeqCommit, OpCovOutputCount, OpCovOutputIdx,
        OpData32, OpDrop, OpDup, OpElse, OpEndIf, OpEqual, OpEqualVerify, OpFromAltStack, OpIf,
        OpInputCovenantId, OpNot, OpNumEqual, OpNumEqualVerify, OpSHA256, OpSwap, OpToAltStack,
        OpTrue, OpTxInputIndex, OpTxInputScriptSigLen, OpTxInputScriptSigSubstr, OpTxOutputAmount,
        OpTxOutputSpk, OpTxOutputSpkSubstr, OpVerify, OpZkPrecompile,
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
    /// Sompi value the script enforces on the permission-exit output (output index 1) when the
    /// batch's `permission_spk_hash` is non-zero. Baked into the redeem script so this value is
    /// part of the covenant SPK identity; the host builder must emit exactly this value on
    /// output 1.
    pub permission_output_value: u64,
}

/// Default permission-exit output value (0.5 KAS). Bundles a sane default for typical
/// deployments; per-covenant overrides go in [`RedeemPins::permission_output_value`].
pub const DEFAULT_PERMISSION_OUTPUT_VALUE: u64 = 50_000_000;

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
    build_and_hash_journal(&mut b, pins);

    match pins.zk_tag {
        ZkTag::R0Succinct => {
            verify_risc0_succinct(&mut b, pins.program_id, pins.control_id, pins.hashfn)
        }
        ZkTag::Groth16 => panic!("groth16 settlement not wired yet"),
    }

    verify_input_index_zero(&mut b);
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
    // Dev script has no journal, no exits - the covenant always emits exactly one output, and
    // it must be at tx-output index 0 (so the SPK check above lands on the covenant-bound
    // output rather than on a host-controlled non-bound sibling).
    b.add_op(OpTxInputIndex).unwrap();
    b.add_op(OpInputCovenantId).unwrap();
    b.add_op(OpCovOutputCount).unwrap();
    b.add_i64(1).unwrap();
    b.add_op(OpEqualVerify).unwrap();
    verify_cov_output_at_idx(&mut b, 0, 0);
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
/// SPK layout: `version(2 BE) | OpBlake2b(1) | OpData32(1) | hash(32) | OpEqual(1)` = 37 bytes.
/// Version is encoded big-endian to match `ScriptPublicKey::to_bytes()`, which is what
/// `OpTxOutputSpk` pushes onto the stack; using little-endian here would silently work for the
/// current `MAX_SCRIPT_PUBLIC_KEY_VERSION = 0` but break the moment that constant changes.
fn hash_redeem_to_spk(b: &mut ScriptBuilder) {
    b.add_op(OpBlake2b).unwrap();
    let mut header = [0u8; 4];
    header[0..2].copy_from_slice(
        &kaspa_consensus_core::constants::MAX_SCRIPT_PUBLIC_KEY_VERSION.to_be_bytes(),
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
/// 160-byte state preimage, appends the input's covenant id (→ 192B), the script-embedded
/// `tx_image_id` (→ 224B), the 32-byte permission commitment (→ 256B) via
/// `verify_outputs_and_append_perm_hash`, then SHA-256s the result.
///
/// Alt layout going in (top→bottom):
/// `[new_lane_tip, new_state, new_seq_commit, prev_lane_tip, prev_state]`
///
/// The 256-byte preimage byte order matches `StateTransition::encode` field-for-field:
/// `prev_state || prev_lane_tip || new_state || new_lane_tip || new_seq_commit || covenant_id ||
///  tx_image_id || permission_spk_hash`.
///
/// Output: `[..., journal_hash]`
fn build_and_hash_journal(b: &mut ScriptBuilder, pins: &RedeemPins<'_>) {
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
    b.add_data(pins.tx_image_id).unwrap();
    b.add_op(OpCat).unwrap();

    // Branch on covenant output count: append permission_spk_hash (count == 2) or 32 zero
    // bytes (count == 1) → 256B preimage matching the guest's journal encoding.
    verify_outputs_and_append_perm_hash(b, pins);

    b.add_op(OpSHA256).unwrap();
}

/// Stack precondition: `[..., preimage224]`. Postcondition: `[..., preimage256]`.
///
/// Reads `OpCovOutputCount` and branches:
/// - `count == 2`: pins the first covenant output to tx-output index 0 and the second to tx-output
///   index 1, asserts `outputs[1].value == pins.permission_output_value`, extracts the 32-byte P2SH
///   script hash from `outputs[1].script_public_key`, rebuilds the full SPK and asserts it matches
///   the actual one, then appends the extracted hash to the preimage.
/// - `count == 1`: pins the sole covenant output to tx-output index 0, then appends 32 zero bytes
///   (matching how the guest encodes a no-exit batch's `permission_spk_hash`).
/// - any other count: fails (`OpNumEqualVerify` in the else branch).
///
/// The `OpCovOutputIdx` pinning is what binds the continuation/permission SPK checks to the
/// *covenant-bound* outputs: without it, a host could leave output 0's covenant binding off and
/// place it on some other index, breaking the rollup chain (the SPK check at output 0 would
/// still pass, but the covenant-bound output that actually flows forward would have a
/// host-controlled SPK).
///
/// The count==2 branch also rejects an extracted hash of `[0; 32]`. `[0; 32]` is the
/// guest's sentinel for "no exits", so a count==2 spend with that hash would commit dust to
/// `permission_spk([0; 32])`, an unspendable script (blake2b is one-way; no preimage hashes
/// to all zeros). Without this reject, a buggy or adversarial host could permanently burn the
/// operator's `pins.permission_output_value` while still passing the journal binding (the
/// journal also commits `[0; 32]` in that case, so the preimage matches). Rejecting it
/// in-script pins the invariant `journal[permission_spk_hash] == [0; 32]` ⇔ `count == 1`.
fn verify_outputs_and_append_perm_hash(b: &mut ScriptBuilder, pins: &RedeemPins<'_>) {
    b.add_op(OpTxInputIndex).unwrap();
    b.add_op(OpInputCovenantId).unwrap();
    b.add_op(OpCovOutputCount).unwrap();
    // Stack: [..., preimage224, count]

    b.add_op(OpDup).unwrap();
    b.add_i64(2).unwrap();
    b.add_op(OpNumEqual).unwrap();
    // Stack: [..., preimage224, count, count == 2]

    b.add_op(OpIf).unwrap();
    {
        // count == 2 branch: permission-exit output is present at index 1.
        b.add_op(OpDrop).unwrap(); // discard the duped count
        // Stack: [..., preimage224]

        // ---- pin both covenant-bound outputs to fixed indices ----
        // Without this, a host could attach the covenant binding to outputs other than 0/1
        // (breaking the rollup chain while still satisfying the SPK / hash extraction below
        // which read outputs by absolute index).
        verify_cov_output_at_idx(b, 0, 0);
        verify_cov_output_at_idx(b, 1, 1);

        // ---- enforce output[1].value == pins.permission_output_value ----
        b.add_i64(1).unwrap();
        b.add_op(OpTxOutputAmount).unwrap();
        // Stack: [..., preimage224, out1_value]
        b.add_i64(pins.permission_output_value as i64).unwrap();
        b.add_op(OpEqualVerify).unwrap();
        // Stack: [..., preimage224]

        // ---- extract the 32-byte script hash from outputs[1].script_public_key ----
        // SPK layout: version(2 BE) | OpBlake2b(1) | OpData32(1) | hash(32) | OpEqual(1) = 37B.
        // `OpTxOutputSpkSubstr` reads the version-prefixed bytes, so [4..36] is the hash.
        b.add_i64(1).unwrap(); // output index
        b.add_i64(4).unwrap(); // start
        b.add_i64(36).unwrap(); // end (exclusive)
        b.add_op(OpTxOutputSpkSubstr).unwrap();
        // Stack: [..., preimage224, hash32]

        // ---- reject hash == [0; 32] ----
        // `[0; 32]` is the guest's no-exits sentinel and must always take the count==1 path.
        // Reaching count==2 with a zero hash would otherwise satisfy the journal binding (the
        // journal also commits `[0; 32]`) while paying dust to `permission_spk([0; 32])`, an
        // unspendable script that permanently locks the operator's permission-output value.
        b.add_op(OpDup).unwrap();
        b.add_data(&[0u8; 32]).unwrap();
        b.add_op(OpEqual).unwrap();
        b.add_op(OpNot).unwrap();
        b.add_op(OpVerify).unwrap();
        // Stack: [..., preimage224, hash32]

        // ---- rebuild the full 37-byte P2SH SPK and assert it matches the actual one ----
        b.add_op(OpDup).unwrap();
        // Stack: [..., preimage224, hash32, hash32]

        // Big-endian to match `ScriptPublicKey::to_bytes()` (the wire-format used by
        // `OpTxOutputSpk`). See `hash_redeem_to_spk` for the same rationale.
        let mut header = [0u8; 4];
        header[0..2].copy_from_slice(
            &kaspa_consensus_core::constants::MAX_SCRIPT_PUBLIC_KEY_VERSION.to_be_bytes(),
        );
        header[2] = OpBlake2b;
        header[3] = OpData32;
        b.add_data(&header).unwrap();
        b.add_op(OpSwap).unwrap();
        b.add_op(OpCat).unwrap();
        // Stack: [..., preimage224, hash32, header4 || hash32] = 36B partial SPK

        b.add_data(&[OpEqual]).unwrap();
        b.add_op(OpCat).unwrap();
        // Stack: [..., preimage224, hash32, header4 || hash32 || OpEqual] = full 37B P2SH SPK

        b.add_i64(1).unwrap();
        b.add_op(OpTxOutputSpk).unwrap();
        b.add_op(OpEqualVerify).unwrap();
        // Stack: [..., preimage224, hash32]

        // ---- append the hash to the preimage → 256B ----
        b.add_op(OpCat).unwrap();
        // Stack: [..., preimage256]
    }
    b.add_op(OpElse).unwrap();
    {
        // count == 1 branch: no permission output; append 32 zero bytes to keep the preimage
        // fixed at 256B (matches the guest's `permission_spk_hash = [0; 32]` encoding).
        b.add_i64(1).unwrap();
        b.add_op(OpNumEqualVerify).unwrap();
        // Stack: [..., preimage224]

        // Pin the sole covenant output to tx-output index 0 (same rationale as the count==2
        // branch: the SPK check earlier in the script reads `outputs[0].script_public_key`
        // by absolute index, so this binds that index to the covenant-bound output).
        verify_cov_output_at_idx(b, 0, 0);

        b.add_data(&[0u8; 32]).unwrap();
        b.add_op(OpCat).unwrap();
        // Stack: [..., preimage256]
    }
    b.add_op(OpEndIf).unwrap();
}

/// Asserts that the `k`-th covenant-bound output (0-indexed within the input's covenant_id
/// group) is at transaction-output index `expected_idx`. Used to pin the continuation /
/// permission outputs to fixed tx-output positions so the SPK and value checks elsewhere in
/// the script (which read outputs by absolute index) land on the covenant-bound ones rather
/// than on host-controlled non-bound siblings.
fn verify_cov_output_at_idx(b: &mut ScriptBuilder, k: i64, expected_idx: i64) {
    b.add_op(OpTxInputIndex).unwrap();
    b.add_op(OpInputCovenantId).unwrap();
    b.add_i64(k).unwrap();
    b.add_op(OpCovOutputIdx).unwrap();
    b.add_i64(expected_idx).unwrap();
    b.add_op(OpEqualVerify).unwrap();
}

/// Asserts that the spending input is at index 0 (covenant always lives at input 0).
fn verify_input_index_zero(b: &mut ScriptBuilder) {
    b.add_op(OpTxInputIndex).unwrap();
    b.add_i64(0).unwrap();
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
        RedeemPins {
            program_id,
            tx_image_id,
            control_id,
            hashfn: 1,
            zk_tag: ZkTag::R0Succinct,
            permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
        }
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
