//! Permission (exit) redeem script construction (`no_std`, byte-level).
//!
//! [`PermissionTreeAccumulator`](crate::PermissionTreeAccumulator) commits a bundle's exits as the
//! P2SH script-hash of a *permission redeem script*: a Kaspa script that, when an exit holder later
//! spends the permission UTXO, enforces a Merkle-proof withdrawal against the committed exit tree.
//! This module builds that redeem script as raw bytes and hashes it to its P2SH script-hash.
//!
//! The build runs in the batch-processor guest, so it cannot use the host-only
//! `kaspa-txscript::ScriptBuilder`; the [`PermRedeemScript`] extension trait emits the opcodes
//! directly. The bytes are a port of rusty-kaspa's `zk-covenant-rollup` `permission_script.rs` /
//! `p2sh.rs`; they must stay byte-identical to what the on-chain script engine and the host-side
//! settlement builder produce.
//!
//! TODO(no_std-kaspa): once `kaspa-txscript` is `no_std`-compatible, replace this hand-rolled
//! emission with its `ScriptBuilder` and `pay_to_script_hash_script`; that deletes the opcode table
//! and the `PermRedeemScript` encoders, and stops us drifting from the canonical script
//! construction.

use alloc::vec::Vec;

use crate::permission_tags::PermNode;

/// Maximum number of *delegate inputs* the permission script will sum over.
///
/// A delegate input is an extra funding UTXO locked under the same covenant that tops up a
/// withdrawal payout beyond what the permission UTXO itself holds. The script unrolls a
/// `1..=N` loop that inspects each candidate input, so this constant directly sizes the
/// script, and, because the script hash is the on-chain commitment, it is a protocol
/// constant the guest (this code) and the host-side settlement builder must agree on.
pub const MAX_DELEGATE_INPUTS: usize = 8;

/// Maximum transaction outputs the permission script permits (the `OpTxOutputCount` ceiling).
///
/// The script can emit up to three output kinds: output 0 (the withdrawal payout); output 1
/// (the P2SH continuation re-committing the still-unclaimed exits); and a delegate-change
/// output at `1 + CovOutCount`. Four leaves one slot of headroom.
const MAX_OUTPUTS: i64 = 4;

const OP_FALSE: u8 = 0x00;
const OP_TRUE: u8 = 0x51;
const OP_IF: u8 = 0x63;
const OP_ELSE: u8 = 0x67;
const OP_ENDIF: u8 = 0x68;
const OP_VERIFY: u8 = 0x69;
const OP_TOALTSTACK: u8 = 0x6b;
const OP_FROMALTSTACK: u8 = 0x6c;
const OP_DROP: u8 = 0x75;
const OP_DUP: u8 = 0x76;
const OP_NIP: u8 = 0x77;
const OP_OVER: u8 = 0x78;
const OP_PICK: u8 = 0x79;
const OP_ROLL: u8 = 0x7a;
const OP_ROT: u8 = 0x7b;
const OP_SWAP: u8 = 0x7c;
const OP_CAT: u8 = 0x7e;
const OP_EQUAL: u8 = 0x87;
const OP_EQUALVERIFY: u8 = 0x88;
const OP_1SUB: u8 = 0x8c;
const OP_ADD: u8 = 0x93;
const OP_SUB: u8 = 0x94;
const OP_NUMEQUALVERIFY: u8 = 0x9d;
const OP_GREATERTHAN: u8 = 0xa0;
const OP_GREATERTHANOREQUAL: u8 = 0xa2;
const OP_SHA256: u8 = 0xa8;
const OP_BLAKE2B: u8 = 0xaa;
const OP_TXINPUTCOUNT: u8 = 0xb3;
const OP_TXOUTPUTCOUNT: u8 = 0xb4;
const OP_TXINPUTINDEX: u8 = 0xb9;
const OP_TXINPUTSCRIPTSIGSUBSTR: u8 = 0xbc;
const OP_TXINPUTAMOUNT: u8 = 0xbe;
const OP_TXINPUTSPK: u8 = 0xbf;
const OP_TXOUTPUTAMOUNT: u8 = 0xc2;
const OP_TXOUTPUTSPK: u8 = 0xc3;
const OP_TXINPUTSCRIPTSIGLEN: u8 = 0xc9;
const OP_NUM2BIN: u8 = 0xcd;
const OP_INPUTCOVENANTID: u8 = 0xcf;
const OP_COVOUTCOUNT: u8 = 0xd2;
const OP_COVOUTPUTIDX: u8 = 0xd3;

/// Delegate entry script bytes **before** the 32-byte covenant_id.
const DELEGATE_SCRIPT_PREFIX: [u8; 7] = [0xb9, 0x00, 0xa0, 0x69, 0x00, 0xcf, 0x20];

/// Delegate entry script bytes **after** the 32-byte covenant_id.
const DELEGATE_SCRIPT_SUFFIX: [u8; 14] =
    [0x88, 0x00, 0x00, 0xc9, 0x76, 0x52, 0x94, 0x7c, 0xbc, 0x02, 0x51, 0x75, 0x88, 0x51];

/// Extension trait that appends the permission redeem script's phases to a byte buffer.
///
/// Each `emit_*` method writes one phase; the paired `*_LEN` associated const is that phase's
/// depth-independent byte count. [`perm_redeem_script_len`] sums them (via [`Self::FIXED_LEN`])
/// to get the script length in closed form, without building it. The counts are hand-derived
/// from the method bodies and cross-checked against the real builder by
/// `const_fn_length_matches_builder_for_all_depths`.
trait PermRedeemScript {
    /// Byte count of `emit_prefix`: `OpData32 | root(32) | OpData8 | unclaimed_count(8 LE)`.
    const PREFIX_LEN: usize = 42;
    /// Byte count of `emit_phase2_stash`.
    const PHASE2_STASH_LEN: usize = 2;
    /// Byte count of `emit_validate_amounts`.
    const VALIDATE_AMOUNTS_LEN: usize = 13;
    /// Byte count of `emit_verify_withdrawal`.
    const VERIFY_WITHDRAWAL_LEN: usize = 13;
    /// Byte count of `emit_compute_leaf_hashes`.
    const COMPUTE_LEAF_HASHES_LEN: usize = 42;
    /// Byte count of `emit_verify_old_root`'s fixed tail (excludes the `depth` Merkle steps).
    const VERIFY_OLD_ROOT_TAIL_LEN: usize = 12;
    /// Byte count of `emit_compute_new_unclaimed`.
    const COMPUTE_NEW_UNCLAIMED_LEN: usize = 13;
    /// Byte count of `emit_verify_outputs`, excluding [`Self::EMBEDDED_LEN_PUSH`].
    const VERIFY_OUTPUTS_FIXED_LEN: usize = 88;
    /// Byte count of `emit_verify_delegate_balance` (`MAX_DELEGATE_INPUTS` fully unrolled).
    const VERIFY_DELEGATE_BALANCE_LEN: usize = 214;
    /// Byte count of `emit_trailer`.
    const TRAILER_LEN: usize = 3;
    /// Byte count of `emit_merkle_step`, emitted `2 * depth` times across the two Merkle walks.
    const MERKLE_STEP_LEN: usize = 11;

    /// Bytes emitted for the one self-referential `push_i64(PREFIX_LEN - total_len)` in
    /// `emit_verify_outputs`.
    ///
    /// For every `depth` in `1..=PERM_MAX_DEPTH` the total script length lands in `[467, 1149]`, so
    /// the pushed magnitude `total_len - 42` is in `[425, 1107]`, always two little-endian bytes
    /// with the high bit clear, so `push_i64` emits 1 length-prefix byte + 2 magnitude bytes = 3,
    /// with no sign byte. Past depth ~1450 the magnitude would need a sign byte and this constant
    /// would have to change.
    const EMBEDDED_LEN_PUSH: usize = 3;

    /// Depth-independent byte count of the redeem script, the sum of every fixed-size phase.
    const FIXED_LEN: usize = Self::PREFIX_LEN
        + Self::PHASE2_STASH_LEN
        + Self::VALIDATE_AMOUNTS_LEN
        + Self::VERIFY_WITHDRAWAL_LEN
        + Self::COMPUTE_LEAF_HASHES_LEN
        + Self::VERIFY_OLD_ROOT_TAIL_LEN
        + Self::COMPUTE_NEW_UNCLAIMED_LEN
        + Self::VERIFY_OUTPUTS_FIXED_LEN
        + Self::VERIFY_DELEGATE_BALANCE_LEN
        + Self::TRAILER_LEN
        + Self::EMBEDDED_LEN_PUSH;

    /// Pushes `data` with a length prefix (len ≤ 75).
    fn push_data(&mut self, data: &[u8]);

    /// Pushes an i64 using minimal script-number encoding, matching `ScriptBuilder::add_i64`:
    /// `0` → `Op0`, `-1` → `Op1Negate`, `1..=16` → `Op1..Op16`, otherwise minimal LE with a
    /// sign bit via [`push_data`](Self::push_data).
    fn push_i64(&mut self, val: i64);

    /// Phase 1: `OpData32 | root | OpData8 | unclaimed_count(8 LE)`.
    fn emit_prefix(&mut self, root: &[u8; 32], unclaimed_count: u64);
    /// Phase 2: stash the embedded root + unclaimed_count to the alt stack.
    fn emit_phase2_stash(&mut self);
    /// Phase 3: validate `deduct > 0` and `amount >= deduct`.
    fn emit_validate_amounts(&mut self);
    /// Phase 4: verify output 0's SPK matches the leaf's SPK.
    fn emit_verify_withdrawal(&mut self);
    /// Phase 5: compute old and new leaf hashes.
    fn emit_compute_leaf_hashes(&mut self);
    /// Phase 6: verify the old root matches the embedded root (`depth` Merkle steps + tail).
    fn emit_verify_old_root(&mut self, depth: usize);
    /// Phase 7: compute the new root from new_leaf (`depth` Merkle steps).
    fn emit_compute_new_root(&mut self, depth: usize);
    /// Phase 8: compute new_unclaimed.
    fn emit_compute_new_unclaimed(&mut self);
    /// Phase 9: verify transaction outputs.
    fn emit_verify_outputs(&mut self, redeem_script_len: i64);
    /// Phase 10: verify delegate input/output balance.
    fn emit_verify_delegate_balance(&mut self);
    /// Trailing `OP_TRUE` (P2SH) followed by the `OP_TRUE OP_DROP` domain suffix.
    fn emit_trailer(&mut self);
    /// One Merkle walk step: combine the current hash with a sibling.
    fn emit_merkle_step(&mut self);
    /// Hash a redeem script → P2SH SPK bytes (version-prefixed):
    /// `version(2B LE) | OpBlake2b | OpData32 | blake2b(redeem) | OpEqual`.
    fn emit_hash_redeem_to_spk(&mut self);
}

impl PermRedeemScript for Vec<u8> {
    fn push_data(&mut self, data: &[u8]) {
        debug_assert!(data.len() <= 75, "push_data only supports len ≤ 75");
        self.push(data.len() as u8);
        self.extend_from_slice(data);
    }

    fn push_i64(&mut self, val: i64) {
        match val {
            0 => self.push(0x00),
            -1 => self.push(0x4f),
            v @ 1..=16 => self.push(0x50 + v as u8),
            _ => {
                let negative = val < 0;
                let mut abs_val = if negative { (-(val as i128)) as u64 } else { val as u64 };

                let mut buf = [0u8; 9];
                let mut len = 0usize;
                while abs_val > 0 {
                    buf[len] = (abs_val & 0xFF) as u8;
                    abs_val >>= 8;
                    len += 1;
                }

                if buf[len - 1] & 0x80 != 0 {
                    buf[len] = if negative { 0x80 } else { 0x00 };
                    len += 1;
                } else if negative {
                    buf[len - 1] |= 0x80;
                }

                self.push_data(&buf[..len]);
            }
        }
    }

    fn emit_prefix(&mut self, root: &[u8; 32], unclaimed_count: u64) {
        self.push(0x20); // OpData32
        self.extend_from_slice(root);
        self.push(0x08); // OpData8
        self.extend_from_slice(&unclaimed_count.to_le_bytes());
    }

    fn emit_phase2_stash(&mut self) {
        self.push(OP_TOALTSTACK); // stash uncl_emb
        self.push(OP_TOALTSTACK); // stash root_emb
    }

    fn emit_validate_amounts(&mut self) {
        // verify deduct > 0
        self.push(OP_DUP);
        self.push_i64(0);
        self.push(OP_GREATERTHAN);
        self.push(OP_VERIFY);

        // stash deduct to alt for later use
        self.push(OP_DUP);
        self.push(OP_TOALTSTACK);

        // new_amount = amount - deduct; verify >= 0
        self.push(OP_OVER);
        self.push(OP_SWAP);
        self.push(OP_SUB);
        self.push(OP_DUP);
        self.push_i64(0);
        self.push(OP_GREATERTHANOREQUAL);
        self.push(OP_VERIFY);
    }

    fn emit_verify_withdrawal(&mut self) {
        // bring spk to top
        self.push_i64(2);
        self.push(OP_ROLL);

        self.push(OP_DUP);

        // prepend SPK version prefix (0x0000 for version 0)
        self.push_data(&0u16.to_le_bytes());
        self.push(OP_SWAP);
        self.push(OP_CAT);

        // compare with output 0's SPK
        self.push_i64(0);
        self.push(OP_TXOUTPUTSPK);
        self.push(OP_EQUALVERIFY);

        // restore stack: spk back to depth 2
        self.push(OP_ROT);
        self.push(OP_ROT);
    }

    fn emit_compute_leaf_hashes(&mut self) {
        // is_zero = (new_amount == 0), stash
        self.push(OP_DUP);
        self.push_i64(0);
        self.push(OP_EQUAL);
        self.push(OP_TOALTSTACK);

        // new_amount -> 8-byte LE
        self.push_i64(8);
        self.push(OP_NUM2BIN);

        // dup spk for reuse in old leaf hash
        self.push(OP_ROT);
        self.push(OP_DUP);
        self.push(OP_TOALTSTACK);

        // new leaf (nonzero): SHA256(Leaf || spk || new_amt_8b)
        self.push(OP_SWAP);
        self.push(OP_CAT);
        self.push_data(&[PermNode::Leaf as u8]);
        self.push(OP_SWAP);
        self.push(OP_CAT);
        self.push(OP_SHA256);

        // empty leaf hash
        self.push_data(&[PermNode::Empty as u8]);
        self.push(OP_SHA256);

        // select new_leaf based on is_zero; re-stash is_zero and spk_dup
        self.push(OP_FROMALTSTACK); // spk_dup
        self.push(OP_FROMALTSTACK); // is_zero
        self.push(OP_DUP);
        self.push(OP_TOALTSTACK); // re-stash is_zero
        self.push(OP_SWAP);
        self.push(OP_TOALTSTACK); // re-stash spk_dup

        self.push(OP_IF);
        self.push(OP_NIP); // is_zero true: keep empty_h, drop new_leaf_nz
        self.push(OP_ELSE);
        self.push(OP_DROP); // is_zero false: keep new_leaf_nz, drop empty_h
        self.push(OP_ENDIF);

        self.push(OP_TOALTSTACK); // stash new_leaf

        // old leaf hash: SHA256(Leaf || spk || amount)
        self.push(OP_FROMALTSTACK); // new_leaf
        self.push(OP_FROMALTSTACK); // spk_dup

        self.push(OP_ROT); // [new_leaf, spk_dup, amount]
        self.push(OP_CAT); // [new_leaf, spk_dup||amount]
        self.push_data(&[PermNode::Leaf as u8]);
        self.push(OP_SWAP);
        self.push(OP_CAT);
        self.push(OP_SHA256);

        // swap and stash new_leaf
        self.push(OP_SWAP);
        self.push(OP_TOALTSTACK);
    }

    fn emit_verify_old_root(&mut self, depth: usize) {
        for _ in 0..depth {
            self.emit_merkle_step();
        }

        // Restore from alt stack.
        self.push(OP_FROMALTSTACK); // new_leaf
        self.push(OP_FROMALTSTACK); // is_zero
        self.push(OP_FROMALTSTACK); // deduct
        self.push(OP_FROMALTSTACK); // root_emb

        // Bring computed_old_root to top (position 4 from top).
        self.push_i64(4);
        self.push(OP_ROLL);
        self.push(OP_EQUALVERIFY);

        // Re-stash deduct and is_zero, leave new_leaf on main.
        self.push(OP_ROT);
        self.push(OP_SWAP);
        self.push(OP_TOALTSTACK); // stash deduct
        self.push(OP_SWAP);
        self.push(OP_TOALTSTACK); // stash is_zero
    }

    fn emit_compute_new_root(&mut self, depth: usize) {
        for _ in 0..depth {
            self.emit_merkle_step();
        }
    }

    fn emit_compute_new_unclaimed(&mut self) {
        self.push(OP_FROMALTSTACK); // is_zero
        self.push(OP_FROMALTSTACK); // deduct
        self.push(OP_FROMALTSTACK); // uncl_emb

        // Bring is_zero to top.
        self.push(OP_ROT);

        // new_uncl = if is_zero { uncl - 1 } else { uncl }
        self.push(OP_IF);
        self.push_i64(1);
        self.push(OP_SUB);
        self.push(OP_ELSE);
        // uncl unchanged
        self.push(OP_ENDIF);

        // Convert to 8-byte LE.
        self.push_i64(8);
        self.push(OP_NUM2BIN);

        // Rearrange to [deduct, new_root, new_uncl_8b].
        self.push(OP_ROT);
        self.push(OP_SWAP);
    }

    fn emit_verify_outputs(&mut self, redeem_script_len: i64) {
        // enforce output 0's payout >= deduct (#77). Main: [deduct, new_root, new_uncl_8b].
        self.push(OP_FALSE); // output index 0
        self.push(OP_TXOUTPUTAMOUNT); // [deduct, new_root, new_uncl_8b, out0_amount]
        self.push_i64(3);
        self.push(OP_PICK); // copy deduct to top -> [..., out0_amount, deduct]
        self.push(OP_GREATERTHANOREQUAL); // out0_amount >= deduct
        self.push(OP_VERIFY); // back to [deduct, new_root, new_uncl_8b]

        // enforce output_count <= MAX_OUTPUTS
        self.push(OP_TXOUTPUTCOUNT);
        self.push_i64(MAX_OUTPUTS);
        self.push(OP_GREATERTHAN);
        self.push_i64(0);
        self.push(OP_EQUALVERIFY);

        // check if all exits claimed (new_uncl == 0)
        self.push(OP_DUP);
        self.push_data(&0u64.to_le_bytes());
        self.push(OP_EQUAL);

        self.push(OP_IF);
        {
            // All exits claimed: no continuation output needed.
            self.push(OP_DROP); // drop new_uncl_8b
            self.push(OP_DROP); // drop new_root

            // Verify no covenant continuation outputs remain.
            self.push(OP_TXINPUTINDEX);
            self.push(OP_INPUTCOVENANTID);
            self.push(OP_COVOUTCOUNT);
            self.push_i64(0);
            self.push(OP_EQUALVERIFY);

            // No continuation output holds the permission UTXO's residual rent, so fold it into the
            // final payout: output 0 value >= deduct + input 0 value. Main in/out: [deduct].
            self.push(OP_FALSE); // output index 0
            self.push(OP_TXOUTPUTAMOUNT); // [deduct, out0]
            self.push(OP_OVER); // copy deduct -> [deduct, out0, deduct]
            self.push(OP_TXINPUTINDEX);
            self.push(OP_TXINPUTAMOUNT); // [deduct, out0, deduct, in0]
            self.push(OP_ADD); // [deduct, out0, deduct + in0]
            self.push(OP_GREATERTHANOREQUAL); // out0 >= deduct + in0
            self.push(OP_VERIFY); // back to [deduct]
        }
        self.push(OP_ELSE);
        {
            // Unclaimed exits remain: verify continuation at output 1.

            // Build unclaimed part: [0x08 || new_uncl_8b].
            self.push_i64(8);
            self.push(OP_SWAP);
            self.push(OP_CAT);

            // Build root part: [OpData32 || new_root].
            self.push(OP_SWAP);
            self.push_data(&[0x20u8]);
            self.push(OP_SWAP);
            self.push(OP_CAT);

            // Concat -> new prefix (42B).
            self.push(OP_SWAP);
            self.push(OP_CAT);

            // Extract body+suffix from own sig_script.
            self.push(OP_TXINPUTINDEX);
            self.push(OP_TXINPUTINDEX);
            self.push(OP_TXINPUTSCRIPTSIGLEN);
            self.push_i64(-redeem_script_len + Self::PREFIX_LEN as i64);
            self.push(OP_ADD);
            self.push(OP_TXINPUTINDEX);
            self.push(OP_TXINPUTSCRIPTSIGLEN);
            self.push(OP_TXINPUTSCRIPTSIGSUBSTR);
            self.push(OP_CAT);

            // Hash -> expected SPK.
            self.emit_hash_redeem_to_spk();

            // Verify output 1 SPK matches.
            self.push_i64(1);
            self.push(OP_TXOUTPUTSPK);
            self.push(OP_EQUALVERIFY);

            // Verify exactly one covenant continuation output at output index 1.
            self.push(OP_TXINPUTINDEX);
            self.push(OP_INPUTCOVENANTID);
            self.push(OP_DUP); // [..., covenant_id, covenant_id]
            self.push(OP_COVOUTCOUNT); // [..., covenant_id, count]
            self.push(OP_DUP); // [..., covenant_id, count, count]
            self.push_i64(1);
            self.push(OP_EQUALVERIFY); // [..., covenant_id, count]
            self.push(OP_1SUB); // [..., covenant_id, 0]  (k = count - 1)
            self.push(OP_COVOUTPUTIDX); // [..., output_idx]
            self.push_i64(1);
            self.push(OP_EQUALVERIFY); // [...]

            // The permission UTXO's residual rent must pass through unchanged into the
            // continuation: output 1 value == input 0 value. Main in/out: [deduct].
            self.push(OP_TXINPUTINDEX);
            self.push(OP_TXINPUTAMOUNT); // [deduct, in0]
            self.push_i64(1);
            self.push(OP_TXOUTPUTAMOUNT); // [deduct, in0, out1]
            self.push(OP_NUMEQUALVERIFY); // in0 == out1, back to [deduct]
        }
        self.push(OP_ENDIF);
    }

    fn emit_verify_delegate_balance(&mut self) {
        let n = MAX_DELEGATE_INPUTS;

        // enforce input_count <= N+2
        self.push(OP_TXINPUTCOUNT);
        self.push_i64((n + 2) as i64);
        self.push(OP_GREATERTHAN);
        self.push_i64(0);
        self.push(OP_EQUALVERIFY);

        // Build expected delegate P2SH SPK (37B) from covenant_id.
        self.push(OP_TXINPUTINDEX);
        self.push(OP_INPUTCOVENANTID);
        self.push_data(&DELEGATE_SCRIPT_PREFIX);
        self.push(OP_SWAP);
        self.push(OP_CAT);
        self.push_data(&DELEGATE_SCRIPT_SUFFIX);
        self.push(OP_CAT);
        self.emit_hash_redeem_to_spk();
        self.push(OP_TOALTSTACK);

        // Sum delegate input amounts (unrolled i = 1..=N).
        self.push_i64(0); // accum = 0
        for i in 1..=n {
            self.push(OP_TXINPUTCOUNT);
            self.push_i64((i + 1) as i64);
            self.push(OP_GREATERTHANOREQUAL);
            self.push(OP_IF);
            {
                self.push_i64(i as i64);
                self.push(OP_TXINPUTSPK);
                self.push(OP_FROMALTSTACK);
                self.push(OP_DUP);
                self.push(OP_TOALTSTACK);
                self.push(OP_EQUAL);
                self.push(OP_IF);
                {
                    self.push_i64(i as i64);
                    self.push(OP_TXINPUTAMOUNT);
                    self.push(OP_ADD);
                }
                self.push(OP_ENDIF);
            }
            self.push(OP_ENDIF);
        }

        // Guard: input N+1 must NOT have delegate SPK.
        self.push(OP_TXINPUTCOUNT);
        self.push_i64((n + 2) as i64);
        self.push(OP_GREATERTHANOREQUAL);
        self.push(OP_IF);
        {
            self.push_i64((n + 1) as i64);
            self.push(OP_TXINPUTSPK);
            self.push(OP_FROMALTSTACK);
            self.push(OP_DUP);
            self.push(OP_TOALTSTACK);
            self.push(OP_EQUAL);
            self.push(OP_FALSE);
            self.push(OP_EQUALVERIFY);
        }
        self.push(OP_ENDIF);

        // Compute expected_change = total_input - deduct; verify >= 0.
        self.push(OP_SWAP);
        self.push(OP_SUB);
        self.push(OP_DUP);
        self.push_i64(0);
        self.push(OP_GREATERTHANOREQUAL);
        self.push(OP_VERIFY);

        // Delegate change index = 1 + CovOutCount.
        self.push(OP_TXINPUTINDEX);
        self.push(OP_INPUTCOVENANTID);
        self.push(OP_COVOUTCOUNT);
        self.push_i64(1);
        self.push(OP_ADD);

        // Verify delegate change output if needed.
        self.push(OP_OVER);
        self.push_i64(0);
        self.push(OP_GREATERTHAN);
        self.push(OP_IF);
        {
            // expected_change > 0: verify output[delegate_idx].
            self.push(OP_DUP);
            self.push(OP_TXOUTPUTSPK);
            self.push(OP_FROMALTSTACK);
            self.push(OP_EQUALVERIFY);
            self.push(OP_TXOUTPUTAMOUNT);
            self.push(OP_EQUALVERIFY);
        }
        self.push(OP_ELSE);
        {
            // expected_change == 0: clean up.
            self.push(OP_DROP); // delegate_idx
            self.push(OP_DROP); // expected_change
            self.push(OP_FROMALTSTACK);
            self.push(OP_DROP); // expected_spk
        }
        self.push(OP_ENDIF);
    }

    fn emit_trailer(&mut self) {
        self.push(OP_TRUE); // final TRUE for P2SH
        self.push(OP_TRUE); // domain suffix [OP_1, OP_DROP]
        self.push(OP_DROP);
    }

    fn emit_merkle_step(&mut self) {
        self.push(OP_SWAP);
        self.push(OP_IF);
        // dir == 1: Cat -> sib||current (already in order)
        self.push(OP_ELSE);
        self.push(OP_SWAP);
        // dir == 0: Cat -> current||sib
        self.push(OP_ENDIF);
        self.push(OP_CAT);
        self.push_data(&[PermNode::Branch as u8]);
        self.push(OP_SWAP);
        self.push(OP_CAT);
        self.push(OP_SHA256);
    }

    fn emit_hash_redeem_to_spk(&mut self) {
        self.push(OP_BLAKE2B);
        // TX_VERSION (0) as 2B LE, then OpBlake2b, OpData32.
        self.push_data(&[0x00, 0x00, OP_BLAKE2B, 0x20]);
        self.push(OP_SWAP);
        self.push(OP_CAT);
        self.push_data(&[OP_EQUAL]);
        self.push(OP_CAT);
    }
}

/// Exact byte length of the permission redeem script for a tree of the given `depth`.
///
/// The script embeds its own length, but that length is a pure function of `depth`:
/// `emit_verify_old_root` and `emit_compute_new_root` each emit `depth` Merkle steps, every
/// other phase is fixed-size, and the one self-referential `push_i64` is a constant width over
/// the supported depth range (see `PermRedeemScript::EMBEDDED_LEN_PUSH`). So the length is
/// `FIXED_LEN + 2 * depth * MERKLE_STEP_LEN`, computable without building the script;
/// `build_permission_redeem_script` asserts the built script matches this.
pub const fn perm_redeem_script_len(depth: usize) -> usize {
    <Vec<u8> as PermRedeemScript>::FIXED_LEN
        + 2 * depth * <Vec<u8> as PermRedeemScript>::MERKLE_STEP_LEN
}

/// Unkeyed blake2b-256 of a redeem script: its P2SH script-hash.
///
/// Matches the hashing in `kaspa_txscript::pay_to_script_hash_script`.
pub fn blake2b_script_hash(redeem_script: &[u8]) -> [u8; 32] {
    let hash = blake2b_simd::Params::new().hash_length(32).hash(redeem_script);
    let mut out = [0u8; 32];
    out.copy_from_slice(hash.as_bytes());
    out
}

/// Builds the permission redeem script bytes for a padded tree `root`.
///
/// The script embeds its own byte length (Phase 9 slices its own sig_script by it).
/// [`perm_redeem_script_len`] gives that length in closed form from `depth`, so the script is
/// built exactly once; the `assert_eq!` guards against the length formula drifting from the
/// bytes the builder actually emits.
pub fn build_permission_redeem_script(
    root: &[u8; 32],
    unclaimed_count: u64,
    depth: usize,
) -> Vec<u8> {
    let len = perm_redeem_script_len(depth);
    let script = build_permission_redeem_bytes(root, unclaimed_count, depth, len as i64);
    assert_eq!(script.len(), len, "perm_redeem_script_len disagrees with the builder");
    script
}

/// Builds the permission redeem script for a given embedded length.
///
/// `redeem_script_len` must equal the returned script's length; [`perm_redeem_script_len`]
/// provides it in closed form.
fn build_permission_redeem_bytes(
    root: &[u8; 32],
    unclaimed_count: u64,
    depth: usize,
    redeem_script_len: i64,
) -> Vec<u8> {
    let mut s: Vec<u8> = Vec::with_capacity(redeem_script_len.max(64) as usize);
    s.emit_prefix(root, unclaimed_count);
    s.emit_phase2_stash();
    s.emit_validate_amounts();
    s.emit_verify_withdrawal();
    s.emit_compute_leaf_hashes();
    s.emit_verify_old_root(depth);
    s.emit_compute_new_root(depth);
    s.emit_compute_new_unclaimed();
    s.emit_verify_outputs(redeem_script_len);
    s.emit_verify_delegate_balance();
    s.emit_trailer();
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission_tree::PermissionTreeAccumulator;

    const PERM_MAX_DEPTH: usize = PermissionTreeAccumulator::MAX_DEPTH;

    // The length constants live on the (private) `PermRedeemScript` trait; alias the few the
    // tests touch.
    const PREFIX_LEN: usize = <Vec<u8> as PermRedeemScript>::PREFIX_LEN;
    const MERKLE_STEP_LEN: usize = <Vec<u8> as PermRedeemScript>::MERKLE_STEP_LEN;
    const EMBEDDED_LEN_PUSH: usize = <Vec<u8> as PermRedeemScript>::EMBEDDED_LEN_PUSH;

    /// Loop-free byte length `push_i64` emits for a *negative* value of magnitude `abs_val`.
    /// `abs_val` is assumed `> 16` (true for every script-length use), so `push_i64` always
    /// takes its general arm. This is the independent oracle for `EMBEDDED_LEN_PUSH`.
    const fn push_i64_neg_len(abs_val: u64) -> usize {
        let bits = u64::BITS - abs_val.leading_zeros();
        let nbytes = bits.div_ceil(8) as usize;
        let top_byte = abs_val >> ((nbytes - 1) * 8);
        // push_i64 appends a sign byte when the top magnitude byte's MSB is set, otherwise it
        // ORs the sign bit in place. push_data adds 1 length-prefix byte.
        let sign_byte = (top_byte & 0x80 != 0) as usize;
        1 + nbytes + sign_byte
    }

    #[test]
    fn const_fn_length_matches_builder_for_all_depths() {
        // The closed-form length must equal the real builder's output for every depth the
        // accumulator can produce, and must not depend on `root` or `unclaimed_count`.
        for depth in 1..=PERM_MAX_DEPTH {
            let expected = perm_redeem_script_len(depth);
            for (root, uc) in [([0u8; 32], 0u64), ([0xFFu8; 32], u64::MAX), ([0x5Au8; 32], 1)] {
                let built = build_permission_redeem_bytes(&root, uc, depth, expected as i64);
                assert_eq!(built.len(), expected, "depth {depth}: const fn vs builder mismatch");
            }
        }
    }

    #[test]
    fn const_fn_length_is_linear_in_depth() {
        // Closed form: 467 bytes at depth 1, +22 (two Merkle walks) per extra depth.
        assert_eq!(perm_redeem_script_len(1), 467);
        assert_eq!(perm_redeem_script_len(PERM_MAX_DEPTH), 467 + 22 * (PERM_MAX_DEPTH - 1));
        for depth in 1..PERM_MAX_DEPTH {
            assert_eq!(
                perm_redeem_script_len(depth + 1) - perm_redeem_script_len(depth),
                2 * MERKLE_STEP_LEN,
            );
        }
    }

    #[test]
    fn perm_redeem_script_len_is_const_evaluable() {
        // Proves it is genuinely a `const fn` (usable in const context, the whole point).
        const AT_MIN: usize = perm_redeem_script_len(1);
        const AT_MAX: usize = perm_redeem_script_len(PERM_MAX_DEPTH);
        assert_eq!(AT_MIN, 467);
        assert_eq!(AT_MAX, 1149); // 467 + 22 * 31
    }

    #[test]
    fn embedded_length_push_is_three_bytes() {
        // EMBEDDED_LEN_PUSH = 3 holds only while the self-referential push_i64's magnitude
        // stays in [256, 32767] with the high bit clear. Verify that for every supported
        // depth, independently of the builder.
        for depth in 1..=PERM_MAX_DEPTH {
            let total = perm_redeem_script_len(depth) as u64;
            let magnitude = total - PREFIX_LEN as u64;
            assert_eq!(
                push_i64_neg_len(magnitude),
                EMBEDDED_LEN_PUSH,
                "depth {depth}: push_i64 width assumption broken",
            );
        }
    }

    #[test]
    fn push_i64_neg_len_oracle_boundaries() {
        // Sanity-check the oracle itself at the byte-width edges push_i64 cares about.
        assert_eq!(push_i64_neg_len(0x7F), 1 + 1); // 1 byte, MSB clear
        assert_eq!(push_i64_neg_len(0xFF), 1 + 1 + 1); // 1 byte, MSB set -> sign byte
        assert_eq!(push_i64_neg_len(0x100), 1 + 2); // 2 bytes, MSB clear
        assert_eq!(push_i64_neg_len(0x7FFF), 1 + 2); // 2 bytes, top byte 0x7F -> MSB clear
        assert_eq!(push_i64_neg_len(0x8000), 1 + 2 + 1); // 2 bytes, MSB set -> sign byte
        assert_eq!(push_i64_neg_len(0xFFFF), 1 + 2 + 1); // 2 bytes, MSB set
    }

    #[test]
    fn build_permission_redeem_script_self_consistent() {
        // `build_*` asserts the length internally; also confirm the byte length and the
        // depth-independent prefix layout.
        let root = [0xABu8; 32];
        let script = build_permission_redeem_script(&root, 7, 4);
        assert_eq!(script.len(), perm_redeem_script_len(4));
        assert_eq!(script[0], 0x20); // OpData32
        assert_eq!(&script[1..33], &root);
        assert_eq!(script[33], 0x08); // OpData8
        assert_eq!(&script[34..42], &7u64.to_le_bytes());
    }

    #[test]
    fn build_permission_redeem_script_spans_depth_range() {
        // The two depths the accumulator clamps to (`required_depth` floor at min and
        // `PERM_MAX_DEPTH` at max) must build without tripping the internal length assert.
        for depth in [1, PERM_MAX_DEPTH] {
            let script = build_permission_redeem_script(&[0u8; 32], 0, depth);
            assert_eq!(script.len(), perm_redeem_script_len(depth));
        }
    }

    #[test]
    fn blake2b_script_hash_is_deterministic() {
        let a = blake2b_script_hash(b"redeem");
        assert_eq!(a, blake2b_script_hash(b"redeem"));
        assert_ne!(a, blake2b_script_hash(b"other"));
    }
}
