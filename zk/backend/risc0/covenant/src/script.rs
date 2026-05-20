//! P2SH redeem script for the settlement covenant.
//!
//! The script enforces five invariants on any settlement transaction spending the covenant UTXO:
//!
//! 1. **State continuation**: the index-0 covenant output's SPK is a P2SH of this script rebuilt
//!    with the advanced `(new_state, new_lane_tip)` pair pushed as the prefix.
//! 2. **Journal binding**: the ZK proof's committed journal hashes to `sha256(prev_state ||
//!    prev_lane_tip || new_state || new_lane_tip || new_seq_commit || covenant_id || tx_image_id ||
//!    permission_spk_hash || subnetwork_id)` - 276 bytes total. `tx_image_id`, `control_id`,
//!    `hashfn`, `image_id`, and `subnetwork_id` are all hardcoded into the script body, so the
//!    covenant SPK pins the entire verifier identity plus the lane it settles - both the
//!    batch-proof verifier circuit, the inner transaction-processor guest the batch checked
//!    against, and the kaspa subnetwork the prover may not deviate from. The 32 bytes before the
//!    20-byte `subnetwork_id` (`permission_spk_hash`) are either the batch's L2→L1 exit commitment
//!    or `[0; 32]` for a no-exit batch (matching how the guest encodes the field).
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
//!    count==2 branch also explicitly rejects an extracted permission hash of `[0; 32]`; that hash
//!    is the guest's no-exits sentinel and would otherwise lock the operator's permission dust to
//!    an unspendable script. All these rules are enforced inside
//!    `verify_outputs_and_append_perm_hash`.

use kaspa_hashes::Hash;
use kaspa_txscript::{
    opcodes::codes::{
        Op0, Op2Dup, OpAdd, OpBlake2b, OpCat, OpChainblockSeqCommit, OpCovOutputCount,
        OpCovOutputIdx, OpData32, OpDiv, OpDrop, OpDup, OpElse, OpEndIf, OpEqual, OpEqualVerify,
        OpFromAltStack, OpIf, OpInputCovenantId, OpMul, OpNot, OpNumEqual, OpNumEqualVerify, OpRot,
        OpSHA256, OpSize, OpSubstr, OpSwap, OpToAltStack, OpTrue, OpTxInputIndex,
        OpTxInputScriptSigLen, OpTxInputScriptSigSubstr, OpTxOutputAmount, OpTxOutputSpk,
        OpTxOutputSpkSubstr, OpVerify, OpZkPrecompile,
    },
    script_builder::ScriptBuilder,
    zk_precompiles::tags::ZkTag,
};

/// Groth16 verifier-identity constants (compressed VK, control-root halves,
/// bn254 control id, risc0 receipt-claim/output/post digests) baked at build
/// time. Cross-tests in this file re-derive each constant from
/// `risc0-groth16`/`risc0-zkvm` at runtime and assert byte-for-byte equality
/// — so any risc0 release that shifts these values trips the test instead of
/// silently breaking the on-chain Groth16 precompile.
mod groth16_consts {
    include!(concat!(env!("OUT_DIR"), "/groth16_consts.rs"));
}

/// Risc0 succinct verifier-identity constants the on-chain `OpZkPrecompile`
/// (R0Succinct branch) consumes alongside the per-spend witness.
///
/// Circuit-determined (the final recursion ZKR + recursion hash, not the user guest), so
/// they live as build-time consts here instead of being extracted from each receipt. Cross-checked
/// against the live `risc0_circuit_recursion::POSEIDON2_CONTROL_IDS` so any upstream rotation
/// trips at test time rather than silently breaking the on-chain Succinct precompile.
pub mod succinct_consts {
    /// `resolve.zkr` poseidon2 control id.
    ///
    /// Every `ProverOpts::succinct()` proof with at least one assumption (every real batch
    /// composes its per-tx receipts as assumptions) ends with a `resolve.zkr` step, so the
    /// outer receipt's `control_id` is the poseidon2 control id for `resolve.zkr` published
    /// in `risc0_circuit_recursion::control_id::POSEIDON2_CONTROL_IDS`.
    pub const SUCCINCT_CONTROL_ID: [u8; 32] = [
        0x53, 0xa7, 0xb2, 0x3d, 0x07, 0xf9, 0x9e, 0x5d, 0x56, 0x85, 0xe8, 0x58, 0x74, 0xf5, 0x18,
        0x1e, 0x84, 0x86, 0xaa, 0x26, 0x7a, 0x0a, 0xe6, 0x07, 0xff, 0xe9, 0xba, 0x47, 0xc8, 0xbd,
        0xda, 0x4a,
    ];

    /// kaspa `OpZkPrecompile` hashfn tag for risc0 `poseidon2`, the only recursion hash kaspa
    /// supports for succinct receipts.
    pub const SUCCINCT_HASHFN: u8 = 1;
}

/// Redeem script prefix size in bytes.
///
/// Layout (87 bytes total):
///
/// ```text
/// OpData20(1) | subnetwork_id(20) | OpData32(1) | prev_lane_tip(32) | OpData32(1) | prev_state(32)
/// ```
pub const REDEEM_PREFIX_LEN: i64 = 87;

/// Verifier-identity constants hardcoded into the redeem script body and shared by both proof
/// systems. `program_id` and `tx_image_id` pin the verifier and the inner-transaction guest;
/// `subnetwork_id` pins the lane the covenant settles for; `permission_output_value` pins the
/// sompi value emitted on the permission-exit output.
#[derive(Copy, Clone)]
pub struct CommonPins<'a> {
    /// Batch processor guest image id (the proof verifier's image_id pushed before
    /// `OpZkPrecompile`).
    pub program_id: &'a [u8; 32],
    /// Transaction processor guest image id, cat-ed into the journal preimage so the inner
    /// proof verifier identity is constrained on-chain.
    pub tx_image_id: &'a [u8; 32],
    /// Kaspa SubnetworkId this covenant settles for. Pushed in the 87-byte redeem prefix and
    /// re-stashed onto the journal preimage's trailing 20 bytes so any proof that names a
    /// different lane fails the SHA-256 binding.
    pub subnetwork_id: &'a [u8; 20],
    /// Sompi value the script enforces on the permission-exit output (output index 1) when the
    /// batch's `permission_spk_hash` is non-zero. Baked into the redeem script so this value is
    /// part of the covenant SPK identity; the host builder must emit exactly this value on
    /// output 1.
    pub permission_output_value: u64,
}

/// R0Succinct-specific pins. The risc0 succinct verifier-identity constants (`control_id`,
/// `hashfn`) are circuit-determined and live as build-time consts in [`succinct_consts`]; no
/// per-spend pin data beyond [`CommonPins`] is needed.
#[derive(Copy, Clone)]
pub struct SuccinctPins<'a> {
    pub common: CommonPins<'a>,
}

/// Groth16-specific pins. Groth16 verifier constants (compressed VK, control-root halves,
/// bn254 control id, tag digests) are baked into the script body as build-time consts; no
/// per-spend pin data beyond [`CommonPins`] is needed.
#[derive(Copy, Clone)]
pub struct Groth16Pins<'a> {
    pub common: CommonPins<'a>,
}

/// Proof-system-tagged pins fed to [`build_redeem_script`]. The variant determines which
/// `OpZkPrecompile` branch the redeem script terminates in (and thus which witness shape the
/// host must produce — see [`crate::settlement::SettlementWitness`]).
#[derive(Copy, Clone)]
pub enum RedeemPins<'a> {
    Succinct(SuccinctPins<'a>),
    Groth16(Groth16Pins<'a>),
}

impl<'a> RedeemPins<'a> {
    /// Shared verifier-identity fields. Use this when the journal-building / output-checking
    /// helpers need fields that don't depend on the proof system.
    pub fn common(&self) -> &CommonPins<'a> {
        match self {
            RedeemPins::Succinct(p) => &p.common,
            RedeemPins::Groth16(p) => &p.common,
        }
    }

    /// Wire-tag for the redeem script's terminal `OpZkPrecompile` (used by the host to pick
    /// the matching witness shape and prover opts).
    pub fn zk_tag(&self) -> ZkTag {
        match self {
            RedeemPins::Succinct(_) => ZkTag::R0Succinct,
            RedeemPins::Groth16(_) => ZkTag::Groth16,
        }
    }
}

/// Default permission-exit output value (0.5 KAS). Bundles a sane default for typical
/// deployments; per-covenant overrides go in [`CommonPins::permission_output_value`].
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

    // 87-byte data prefix (redeem-script-identifying bytes). `subnetwork_id` is pushed last so it
    // lands at the bottom of the alt stash and is popped last by `build_and_hash_journal` to land
    // at the trailing 20 bytes of the 276-byte preimage.
    b.add_data(prev_lane_tip.as_slice()).unwrap();
    b.add_data(prev_state).unwrap();
    b.add_data(pins.common().subnetwork_id).unwrap();

    stash_prev_values(&mut b);
    obtain_new_seq_commitment(&mut b);
    build_next_redeem_prefix(&mut b);
    append_subnetwork_id_to_next_prefix(&mut b, pins.common().subnetwork_id);
    extract_redeem_suffix_and_concat(&mut b, redeem_script_len);
    hash_redeem_to_spk(&mut b);
    verify_output_spk(&mut b);
    build_and_hash_journal(&mut b, pins);

    match pins {
        RedeemPins::Succinct(p) => verify_risc0_succinct(&mut b, p.common.program_id),
        RedeemPins::Groth16(p) => verify_risc0_groth16(&mut b, p.common.program_id),
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
    subnetwork_id: &[u8; 20],
    redeem_script_len: i64,
) -> Vec<u8> {
    let mut b = ScriptBuilder::new();

    // Same 87-byte prefix as the production script
    // (OpData32||prev_lane_tip||OpData32||prev_state||OpData20||subnetwork_id). Dev has no journal
    // binding so the subnetwork_id pin is structural only — included to keep the prefix length in
    // sync with the production script.
    b.add_data(prev_lane_tip.as_slice()).unwrap();
    b.add_data(prev_state).unwrap();
    b.add_data(subnetwork_id).unwrap();

    stash_prev_values(&mut b);
    obtain_new_seq_commitment(&mut b);
    verify_claimed_seq_commit(&mut b);
    build_next_redeem_prefix_no_stash(&mut b);
    append_subnetwork_id_to_next_prefix(&mut b, subnetwork_id);
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
pub fn dev_redeem_script_len(prev_state: &[u8; 32], subnetwork_id: &[u8; 20]) -> i64 {
    let mut guess: i64 = 75;
    loop {
        let len = build_dev_redeem_script(prev_state, &Hash::default(), subnetwork_id, guess).len()
            as i64;
        if len == guess {
            return len;
        }
        guess = len;
    }
}

/// Stack: `[..., new_lane_tip, new_state, block_prove_to, prev_lane_tip, prev_state,
/// subnetwork_id]` →      `[..., new_lane_tip, new_state, block_prove_to]`  alt: `[subnetwork_id,
/// prev_state, prev_lane_tip]` (bottom→top)
fn stash_prev_values(b: &mut ScriptBuilder) {
    b.add_op(OpToAltStack).unwrap();
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

/// Dev-mode only. Pops and discards `prev_state`, `prev_lane_tip`, and `subnetwork_id` from the alt
/// stack (production consumes them inside `build_and_hash_journal`; dev has no journal).
fn drop_stashed_prev_values(b: &mut ScriptBuilder) {
    b.add_op(OpFromAltStack).unwrap();
    b.add_op(OpDrop).unwrap();
    b.add_op(OpFromAltStack).unwrap();
    b.add_op(OpDrop).unwrap();
    b.add_op(OpFromAltStack).unwrap();
    b.add_op(OpDrop).unwrap();
}

/// Appends the lane's 21-byte subnetwork-id prefix (`OpData20 || subnetwork_id`) to the
/// top-of-stack 66-byte trailing prefix produced by [`build_next_redeem_prefix`], yielding the full
/// 87-byte next-spend prefix. The lane id is the same for every spend of a given covenant (the SPK
/// pins it), so it is pushed from the script body as a constant.
///
/// Stack: `[..., prefix66]` → `[..., prefix66 || (OpData20||subnetwork_id)]` = 87B
fn append_subnetwork_id_to_next_prefix(b: &mut ScriptBuilder, subnetwork_id: &[u8; 20]) {
    b.add_data(subnetwork_id).unwrap();
    b.add_op(OpCat).unwrap();
}

/// Constructs the 66-byte trailing redeem-script prefix for the next covenant UTXO (embedding
/// `new_state` and `new_lane_tip` so the next spend can verify continuity) and stashes
/// `new_seq_commit`, `new_state`, `new_lane_tip` on the alt stack for later journal
/// construction. The leading 21-byte `OpData20 || subnetwork_id` is appended separately by
/// [`append_subnetwork_id_to_next_prefix`] to extend the result to the full 87B prefix.
///
/// Stack: `[..., new_lane_tip, new_state, new_seq_commit]` alt: `[subnetwork_id, prev_state,
/// prev_lane_tip]` →      `[..., prefix(66)]`
///        alt: `[subnetwork_id, prev_state, prev_lane_tip, new_seq_commit, new_state,
/// new_lane_tip]`
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

/// Consumes the alt-stack stash, assembles the 160-byte state preimage, appends the input's
/// covenant id (→ 192B), the script-embedded `tx_image_id` (→ 224B), the 32-byte permission
/// commitment (→ 256B) via `verify_outputs_and_append_perm_hash`, recovers the bottom-of-alt
/// `subnetwork_id` and appends it (→ 276B), then SHA-256s the result.
///
/// Alt layout going in (top→bottom):
/// `[new_lane_tip, new_state, new_seq_commit, prev_lane_tip, prev_state, subnetwork_id]`
///
/// The 276-byte preimage byte order matches `StateTransition::encode` field-for-field:
/// `prev_state || prev_lane_tip || new_state || new_lane_tip || new_seq_commit || covenant_id ||
///  tx_image_id || permission_spk_hash || subnetwork_id`.
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
    b.add_data(pins.common().tx_image_id).unwrap();
    b.add_op(OpCat).unwrap();

    // Branch on covenant output count: append permission_spk_hash (count == 2) or 32 zero
    // bytes (count == 1) → 256B preimage matching the guest's journal encoding.
    verify_outputs_and_append_perm_hash(b, pins);

    // Append the lane's 20-byte subnetwork_id (recovered from the bottom of the alt stash) → 276B
    // preimage.
    b.add_op(OpFromAltStack).unwrap();
    b.add_op(OpCat).unwrap();

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
        b.add_i64(pins.common().permission_output_value as i64).unwrap();
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
/// Pushes `image_id` from the host pin and `control_id` / `hashfn` from the build-time
/// [`succinct_consts`] in the natural pop order of R0Succinct (top→bottom:
/// `hashfn, control_id, image_id, journal, seal, control_digests, control_index, claim`),
/// then the `ZkTag` byte and `OpZkPrecompile`.
fn verify_risc0_succinct(b: &mut ScriptBuilder, image_id: &[u8; 32]) {
    b.add_data(image_id).unwrap();
    b.add_data(&succinct_consts::SUCCINCT_CONTROL_ID).unwrap();
    b.add_data(&[succinct_consts::SUCCINCT_HASHFN]).unwrap();
    b.add_data(&[ZkTag::R0Succinct as u8]).unwrap();
    b.add_op(OpZkPrecompile).unwrap();
    b.add_op(OpVerify).unwrap();
}

/// Emits the risc0 Groth16 verification.
///
/// Stack arriving here (bot→top):
/// `[..., compressed_proof, journal_hash]`
///
/// Pushes `image_id` from the script body, recomputes the risc0 receipt-claim hash on-stack
/// from `(journal_hash, image_id)`, lays out the 5 Groth16 public inputs in the precompile's
/// pop order (id, claim_left_padded, claim_right_padded, control_root_a1, control_root_a0),
/// then pushes the public-input count, the proof bytes (recovered from alt stack), the
/// compressed verifying key, and the Groth16 tag byte before `OpZkPrecompile`.
///
/// The verifier-identity constants (compressed VK, control-root halves, bn254 control id, the
/// three receipt-tag digests) are baked into the script body via build.rs. See
/// [`groth16_consts`] and the cross-tests in this file.
fn verify_risc0_groth16(b: &mut ScriptBuilder, image_id: &[u8; 32]) {
    use groth16_consts::*;

    // Stack: [..., compressed_proof, journal_hash]
    b.add_data(image_id).unwrap();
    // Stack: [..., compressed_proof, journal_hash, image_id]

    // Move proof to alt stack.
    b.add_op(OpRot).unwrap();
    // Stack: [..., journal_hash, image_id, compressed_proof]
    b.add_op(OpToAltStack).unwrap();
    // Stack: [..., journal_hash, image_id]  alt: [..., compressed_proof]

    // Compute receipt_claim hash from (journal_hash, image_id) on-stack.
    compute_receipt_claim(b);
    // Stack: [..., receipt_claim_hash]  alt: [..., compressed_proof]

    // Public inputs are popped by the precompile as
    // `[a0, a1, claim_right_padded, claim_left_padded, id_bn254_fr]`; push in reverse so the
    // ordering is correct top→bottom.
    b.add_op(OpToAltStack).unwrap();
    // Stack: [...]  alt: [..., compressed_proof, receipt_claim_hash]

    b.add_data(&GROTH16_BN254_CONTROL_ID).unwrap();
    // Stack: [..., id]

    b.add_op(OpFromAltStack).unwrap();
    // Stack: [..., id, receipt_claim_hash]  alt: [..., compressed_proof]

    split_at_mid(b);
    // Stack: [..., id, claim_left, claim_right]

    // Right-pad claim_right (top) to 32 bytes.
    b.add_data(&[0u8; 16]).unwrap();
    b.add_op(OpCat).unwrap();
    // Stack: [..., id, claim_left, claim_right_padded]

    // Pad claim_left in the same way.
    b.add_op(OpSwap).unwrap();
    b.add_data(&[0u8; 16]).unwrap();
    b.add_op(OpCat).unwrap();
    // Stack: [..., id, claim_right_padded, claim_left_padded]

    // Control-root halves.
    b.add_data(&GROTH16_CONTROL_ROOT_A1).unwrap();
    b.add_data(&GROTH16_CONTROL_ROOT_A0).unwrap();
    // Stack: [..., id, c1, c0, a1, a0]

    // Public-input count.
    b.add_i64(GROTH16_PUBLIC_INPUT_COUNT as i64).unwrap();
    // Stack: [..., id, c1, c0, a1, a0, 5]

    // Recover proof from alt stack.
    b.add_op(OpFromAltStack).unwrap();
    // Stack: [..., id, c1, c0, a1, a0, 5, compressed_proof]

    // Verifying key.
    b.add_data(GROTH16_VK_COMPRESSED).unwrap();
    // Stack: [..., 5, compressed_proof, vk]

    // ZkTag byte + precompile + verify.
    b.add_data(&[ZkTag::Groth16 as u8]).unwrap();
    b.add_op(OpZkPrecompile).unwrap();
    b.add_op(OpVerify).unwrap();
}

/// Number of public inputs the risc0 Groth16 verifier consumes (id_bn254_fr,
/// claim_left_padded, claim_right_padded, control_root_a1, control_root_a0). See
/// `risc0-groth16` reference impl.
const GROTH16_PUBLIC_INPUT_COUNT: usize = 5;

/// Inlines the risc0 receipt-claim hash computation. Mirrors
/// `examples/zk-covenant-common/src/groth16.rs::compute_receipt_claim` field-for-field; the
/// preimage layout is risc0's canonical `ReceiptClaim` encoding.
///
/// Stack precondition: `[..., journal_hash, image_id]`
/// Stack postcondition: `[..., receipt_claim_hash]`
fn compute_receipt_claim(b: &mut ScriptBuilder) {
    use groth16_consts::*;

    // --- output_digest = SHA256(OUTPUT_TAG || journal_hash || zeros32 || 2u16_le) ---
    b.add_op(OpToAltStack).unwrap();
    // Stack: [..., journal_hash]  alt: [..., image_id]
    b.add_data(&OUTPUT_TAG_DIGEST).unwrap();
    b.add_op(OpSwap).unwrap();
    b.add_op(OpCat).unwrap();
    // Stack: [..., OUTPUT_TAG || journal_hash]  (64B)
    b.add_data(&[0u8; 32]).unwrap();
    b.add_op(OpCat).unwrap();
    // Stack: [..., OUTPUT_TAG || journal_hash || zeros32]  (96B)
    b.add_data(&2u16.to_le_bytes()).unwrap();
    b.add_op(OpCat).unwrap();
    // Stack: [..., 98B-preimage]
    b.add_op(OpSHA256).unwrap();
    // Stack: [..., output_digest]  alt: [..., image_id]

    // --- receipt_claim = SHA256(RC_TAG || zeros32 || image_id || POST || output_digest ||
    // trailer10) ---
    b.add_data(&RECEIPT_CLAIM_TAG_DIGEST).unwrap();
    b.add_data(&[0u8; 32]).unwrap();
    b.add_op(OpCat).unwrap();
    // Stack: [..., output_digest, RC_TAG || zeros32]  (64B partial)
    b.add_op(OpFromAltStack).unwrap();
    b.add_op(OpCat).unwrap();
    // Stack: [..., output_digest, RC_TAG || zeros32 || image_id]  (96B)
    b.add_data(&POST_DIGEST).unwrap();
    b.add_op(OpCat).unwrap();
    // Stack: [..., output_digest, RC_TAG || zeros32 || image_id || POST]  (128B)
    b.add_op(OpSwap).unwrap();
    b.add_op(OpCat).unwrap();
    // Stack: [..., RC_TAG || ... || POST || output_digest]  (160B)
    b.add_data(&[0u8, 0, 0, 0, 0, 0, 0, 0, 4, 0]).unwrap();
    b.add_op(OpCat).unwrap();
    // Stack: [..., 170B receipt_claim preimage]
    b.add_op(OpSHA256).unwrap();
    // Stack: [..., receipt_claim_hash]
}

/// Splits the top-of-stack byte array at its midpoint. Mirrors
/// `examples/zk-covenant-common/src/script_ext.rs::split_at_mid`.
///
/// Stack precondition: `[..., byte_array]`
/// Stack postcondition: `[..., first_half, second_half]`
fn split_at_mid(b: &mut ScriptBuilder) {
    b.add_op(OpSize).unwrap();
    b.add_i64(2).unwrap();
    b.add_op(OpDiv).unwrap();
    b.add_op(Op2Dup).unwrap();
    b.add_i64(0).unwrap();
    b.add_op(OpSwap).unwrap();
    b.add_op(OpSubstr).unwrap();
    b.add_op(OpRot).unwrap();
    b.add_op(OpRot).unwrap();
    b.add_op(OpDup).unwrap();
    b.add_i64(2).unwrap();
    b.add_op(OpMul).unwrap();
    b.add_op(OpSubstr).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only stable lane id for redeem-script tests (the value doesn't matter for length /
    /// structure assertions, only that it's the same across calls).
    const TEST_SUBNETWORK_ID: [u8; 20] = [0xAA; 20];

    fn common_pins<'a>(program_id: &'a [u8; 32], tx_image_id: &'a [u8; 32]) -> CommonPins<'a> {
        CommonPins {
            program_id,
            tx_image_id,
            subnetwork_id: &TEST_SUBNETWORK_ID,
            permission_output_value: DEFAULT_PERMISSION_OUTPUT_VALUE,
        }
    }

    fn succinct_pins<'a>(program_id: &'a [u8; 32], tx_image_id: &'a [u8; 32]) -> RedeemPins<'a> {
        RedeemPins::Succinct(SuccinctPins { common: common_pins(program_id, tx_image_id) })
    }

    fn groth16_pins<'a>(program_id: &'a [u8; 32], tx_image_id: &'a [u8; 32]) -> RedeemPins<'a> {
        RedeemPins::Groth16(Groth16Pins { common: common_pins(program_id, tx_image_id) })
    }

    #[test]
    fn redeem_script_length_converges_succinct() {
        let state = [0u8; 32];
        let pins = succinct_pins(&[1u8; 32], &[2u8; 32]);
        let len = redeem_script_len(&state, &pins);
        let built = build_redeem_script(&state, &Hash::default(), len, &pins);
        assert_eq!(built.len() as i64, len, "self-referential length must match");
    }

    #[test]
    fn redeem_script_length_converges_groth16() {
        let state = [0u8; 32];
        let pins = groth16_pins(&[1u8; 32], &[2u8; 32]);
        let len = redeem_script_len(&state, &pins);
        let built = build_redeem_script(&state, &Hash::default(), len, &pins);
        assert_eq!(built.len() as i64, len, "self-referential length must match");
    }

    #[test]
    fn redeem_script_length_stable_across_values_succinct() {
        let pins = succinct_pins(&[1u8; 32], &[2u8; 32]);
        let a = redeem_script_len(&[0u8; 32], &pins);
        let b = redeem_script_len(&[0xffu8; 32], &pins);
        assert_eq!(a, b, "length must not depend on prev state");
    }

    #[test]
    fn redeem_script_length_stable_across_values_groth16() {
        let pins = groth16_pins(&[1u8; 32], &[2u8; 32]);
        let a = redeem_script_len(&[0u8; 32], &pins);
        let b = redeem_script_len(&[0xffu8; 32], &pins);
        assert_eq!(a, b, "length must not depend on prev state");
    }

    #[test]
    fn dev_redeem_script_length_converges() {
        let state = [0u8; 32];
        let len = dev_redeem_script_len(&state, &TEST_SUBNETWORK_ID);
        let built = build_dev_redeem_script(&state, &Hash::default(), &TEST_SUBNETWORK_ID, len);
        assert_eq!(built.len() as i64, len, "self-referential length must match");
    }

    #[test]
    fn dev_redeem_script_distinct_from_prod_succinct() {
        let state = [0u8; 32];
        let pins = succinct_pins(&[1u8; 32], &[2u8; 32]);
        let prod_len = redeem_script_len(&state, &pins);
        let prod = build_redeem_script(&state, &Hash::default(), prod_len, &pins);
        let dev_len = dev_redeem_script_len(&state, &TEST_SUBNETWORK_ID);
        let dev = build_dev_redeem_script(&state, &Hash::default(), &TEST_SUBNETWORK_ID, dev_len);
        assert_ne!(prod, dev, "dev and prod scripts must differ");
        assert!(dev.len() < prod.len(), "dev script should be shorter (no journal/zk verify)");
    }

    #[test]
    fn dev_redeem_script_distinct_from_prod_groth16() {
        let state = [0u8; 32];
        let pins = groth16_pins(&[1u8; 32], &[2u8; 32]);
        let prod_len = redeem_script_len(&state, &pins);
        let prod = build_redeem_script(&state, &Hash::default(), prod_len, &pins);
        let dev_len = dev_redeem_script_len(&state, &TEST_SUBNETWORK_ID);
        let dev = build_dev_redeem_script(&state, &Hash::default(), &TEST_SUBNETWORK_ID, dev_len);
        assert_ne!(prod, dev, "dev and prod scripts must differ");
        assert!(dev.len() < prod.len(), "dev script should be shorter (no journal/zk verify)");
    }

    #[test]
    fn succinct_and_groth16_redeem_scripts_differ() {
        let state = [0u8; 32];
        let s = succinct_pins(&[1u8; 32], &[2u8; 32]);
        let g = groth16_pins(&[1u8; 32], &[2u8; 32]);
        let s_len = redeem_script_len(&state, &s);
        let g_len = redeem_script_len(&state, &g);
        let s_script = build_redeem_script(&state, &Hash::default(), s_len, &s);
        let g_script = build_redeem_script(&state, &Hash::default(), g_len, &g);
        assert_ne!(s_script, g_script, "succinct and groth16 redeem scripts must differ");
    }

    /// Cross-test: every constant baked into `groth16_consts` by build.rs must equal what the
    /// live `risc0-groth16` / `risc0-zkvm` libraries produce at runtime. If risc0 rolls a new
    /// verifying key or shifts a tag digest, this test trips before the on-chain Groth16
    /// precompile silently breaks.
    mod groth16_consts_cross_check {
        use ark_bn254::Bn254;
        use ark_groth16::VerifyingKey;
        use ark_serialize::{CanonicalDeserialize, CanonicalSerialize};
        use risc0_zkvm::{
            Groth16ReceiptVerifierParameters, SystemState,
            sha::{Digest, Digestible},
        };
        use sha2::{Digest as _, Sha256};

        use super::super::groth16_consts::*;

        fn runtime_vk_compressed() -> Vec<u8> {
            let vk_risc0 = risc0_groth16::verifying_key();
            let vk_bytes_prefixed_with_len = bincode::serialize(&vk_risc0).unwrap();
            let vk = VerifyingKey::<Bn254>::deserialize_uncompressed(
                &mut &vk_bytes_prefixed_with_len[8..],
            )
            .unwrap();
            let mut out = Vec::new();
            vk.serialize_compressed(&mut out).unwrap();
            out
        }

        fn split_digest(d: Digest) -> ([u8; 32], [u8; 32]) {
            const MID: usize = core::mem::size_of::<Digest>() / 2;
            let (first, second) = d.as_bytes().split_at(MID);
            let pad = |slice: &[u8]| {
                let mut scalar = [0u8; 32];
                scalar[..MID].copy_from_slice(slice);
                scalar
            };
            (pad(first), pad(second))
        }

        fn sha256_of(input: &[u8]) -> [u8; 32] {
            let mut hasher = Sha256::new();
            hasher.update(input);
            hasher.finalize().into()
        }

        #[test]
        fn vk_compressed_matches_risc0() {
            assert_eq!(GROTH16_VK_COMPRESSED, runtime_vk_compressed().as_slice());
        }

        #[test]
        fn bn254_control_id_matches_risc0() {
            let id: [u8; 32] = Groth16ReceiptVerifierParameters::default().bn254_control_id.into();
            assert_eq!(GROTH16_BN254_CONTROL_ID, id);
        }

        #[test]
        fn control_root_halves_match_risc0() {
            let (a0, a1) = split_digest(Groth16ReceiptVerifierParameters::default().control_root);
            assert_eq!(GROTH16_CONTROL_ROOT_A0, a0);
            assert_eq!(GROTH16_CONTROL_ROOT_A1, a1);
        }

        #[test]
        fn receipt_claim_tag_matches_sha256() {
            assert_eq!(RECEIPT_CLAIM_TAG_DIGEST, sha256_of(b"risc0.ReceiptClaim"));
        }

        #[test]
        fn output_tag_matches_sha256() {
            assert_eq!(OUTPUT_TAG_DIGEST, sha256_of(b"risc0.Output"));
        }

        #[test]
        fn post_digest_matches_risc0() {
            let expected: [u8; 32] = SystemState { pc: 0, merkle_root: Digest::ZERO }
                .digest()
                .as_bytes()
                .try_into()
                .unwrap();
            assert_eq!(POST_DIGEST, expected);
        }
    }

    /// Cross-test: the hardcoded succinct verifier-identity constants must equal the live
    /// values risc0 produces. Any risc0 release that rotates `resolve.zkr`'s poseidon2
    /// control id (e.g. a new recursion-circuit version) trips this test rather than
    /// silently breaking on-chain succinct verification.
    mod succinct_consts_cross_check {
        use risc0_circuit_recursion::control_id::POSEIDON2_CONTROL_IDS;

        use super::super::succinct_consts::*;

        #[test]
        fn succinct_control_id_matches_resolve_zkr_poseidon2() {
            let live: [u8; 32] = POSEIDON2_CONTROL_IDS
                .iter()
                .find_map(|(name, digest)| (*name == "resolve.zkr").then_some(*digest))
                .expect("resolve.zkr present in POSEIDON2_CONTROL_IDS")
                .as_bytes()
                .try_into()
                .expect("digest is 32 bytes");
            assert_eq!(
                SUCCINCT_CONTROL_ID, live,
                "SUCCINCT_CONTROL_ID must match the live resolve.zkr poseidon2 control id",
            );
        }
    }
}
