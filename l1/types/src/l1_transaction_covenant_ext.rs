//! Covenant-settlement detection on [`L1Transaction`].
//!
//! A settlement of covenant `X` (see `zk/backend/risc0/covenant/src/settlement.rs`) always:
//!  1. binds at least one output to `X` via `TransactionOutput::covenant`, and
//!  2. ends its input-0 `signature_script` with the four-push tail `[new_lane_tip(32),
//!     new_state(32), block_prove_to(32), prev_redeem(var)]`.
//!
//! The first property is the cheap structural signal we trigger off; the second lets us decode the
//! data the prover needs to know "up to which L1 block has this covenant already been settled by
//! another prover" without inspecting on-chain UTXO state.

use crate::{Hash, L1Transaction, SettlementInfo};

/// Covenant-aware extension methods for [`L1Transaction`].
pub trait L1TransactionCovenantExt {
    /// Decodes `self` as a settlement of `covenant_id`, or `None` if it isn't one.
    fn settlement_info(&self, covenant_id: Hash, containing_block: Hash) -> Option<SettlementInfo>;
}

impl L1TransactionCovenantExt for L1Transaction {
    fn settlement_info(&self, covenant_id: Hash, containing_block: Hash) -> Option<SettlementInfo> {
        if self.outputs.first()?.covenant.as_ref()?.covenant_id != covenant_id {
            return None;
        }

        let input = self.inputs.first()?;
        let tail = sig_script_tail_4(&input.signature_script)?;
        let new_lane_tip: [u8; 32] = tail[0].try_into().ok()?;
        let new_state: [u8; 32] = tail[1].try_into().ok()?;
        let block_prove_to: [u8; 32] = tail[2].try_into().ok()?;

        Some(SettlementInfo {
            tx_id: self.id(),
            containing_block,
            block_prove_to: Hash::from_bytes(block_prove_to),
            new_state,
            new_lane_tip: Hash::from_bytes(new_lane_tip),
        })
    }
}

/// Returns the payloads of the last four data pushes in `script`, or `None` if `script` contains
/// fewer than four pushes or any byte is a non-push opcode.
///
/// Kaspa data-push opcodes (mirrors `kaspa_txscript::opcodes::codes`):
/// - `0x00`: empty push.
/// - `0x01..=0x4b`: literal push, opcode byte is the payload length (`OpData1..OpData75`).
/// - `0x4c`: `OpPushData1`, 1-byte length prefix.
/// - `0x4d`: `OpPushData2`, 2-byte little-endian length prefix.
/// - `0x4e`: `OpPushData4`, 4-byte little-endian length prefix.
fn sig_script_tail_4(script: &[u8]) -> Option<[&[u8]; 4]> {
    let mut tail: [&[u8]; 4] = [&[]; 4];
    let mut count = 0usize;
    let mut i = 0usize;
    while i < script.len() {
        let opcode = script[i];
        i += 1;
        let (len, prefix_bytes) = match opcode {
            0x00 => (0, 0),
            0x01..=0x4b => (opcode as usize, 0),
            0x4c => (*script.get(i)? as usize, 1),
            0x4d => {
                let bytes: [u8; 2] = script.get(i..i + 2)?.try_into().ok()?;
                (u16::from_le_bytes(bytes) as usize, 2)
            }
            0x4e => {
                let bytes: [u8; 4] = script.get(i..i + 4)?.try_into().ok()?;
                (u32::from_le_bytes(bytes) as usize, 4)
            }
            _ => return None,
        };
        i += prefix_bytes;
        let end = i.checked_add(len)?;
        if end > script.len() {
            return None;
        }
        let payload = &script[i..end];
        tail.copy_within(1..4, 0);
        tail[3] = payload;
        count += 1;
        i = end;
    }
    if count < 4 { None } else { Some(tail) }
}

#[cfg(test)]
mod tests {
    use kaspa_consensus_core::{
        constants::TX_VERSION_TOCCATA,
        subnets::SUBNETWORK_ID_NATIVE,
        tx::{
            CovenantBinding, ScriptPublicKey, Transaction, TransactionInput, TransactionOutpoint,
            TransactionOutput,
        },
    };

    use super::*;

    const COVENANT_ID: Hash = Hash::from_bytes([0xAA; 32]);
    const CONTAINING_BLOCK: Hash = Hash::from_bytes([0x44; 32]);
    const NEW_LANE_TIP: [u8; 32] = [0x11; 32];
    const NEW_STATE: [u8; 32] = [0x22; 32];
    const BLOCK_PROVE_TO: [u8; 32] = [0x33; 32];
    const PREV_REDEEM: &[u8] = &[0xCC; 200];

    /// Encodes one data push using the canonical opcode for `data.len()`.
    fn push(buf: &mut Vec<u8>, data: &[u8]) {
        match data.len() {
            0 => buf.push(0x00),
            n if n <= 0x4b => {
                buf.push(n as u8);
                buf.extend_from_slice(data);
            }
            n if n <= 0xff => {
                buf.push(0x4c);
                buf.push(n as u8);
                buf.extend_from_slice(data);
            }
            n if n <= 0xffff => {
                buf.push(0x4d);
                buf.extend_from_slice(&(n as u16).to_le_bytes());
                buf.extend_from_slice(data);
            }
            n => {
                buf.push(0x4e);
                buf.extend_from_slice(&(n as u32).to_le_bytes());
                buf.extend_from_slice(data);
            }
        }
    }

    fn build_sig_script(leading: &[&[u8]]) -> Vec<u8> {
        let mut buf = Vec::new();
        for item in leading {
            push(&mut buf, item);
        }
        push(&mut buf, &NEW_LANE_TIP);
        push(&mut buf, &NEW_STATE);
        push(&mut buf, &BLOCK_PROVE_TO);
        push(&mut buf, PREV_REDEEM);
        buf
    }

    fn settlement_tx(sig_script: Vec<u8>, covenant_id: Hash) -> L1Transaction {
        Transaction::new(
            TX_VERSION_TOCCATA,
            vec![TransactionInput::new(
                TransactionOutpoint::new(Hash::from_bytes([0x66; 32]), 0),
                sig_script,
                0,
                1,
            )],
            vec![TransactionOutput::with_covenant(
                100_000_000,
                ScriptPublicKey::default(),
                Some(CovenantBinding::new(0, covenant_id)),
            )],
            0,
            SUBNETWORK_ID_NATIVE,
            0,
            Vec::new(),
        )
    }

    #[test]
    fn extracts_tail_from_groth16_layout() {
        let sig_script = build_sig_script(&[&[0xEE; 800]]);
        let tx = settlement_tx(sig_script, COVENANT_ID);

        let settlement = tx.settlement_info(COVENANT_ID, CONTAINING_BLOCK).expect("groth16 tail");

        assert_eq!(settlement.tx_id, tx.id());
        assert_eq!(settlement.containing_block, CONTAINING_BLOCK);
        assert_eq!(settlement.new_lane_tip, Hash::from_bytes(NEW_LANE_TIP));
        assert_eq!(settlement.new_state, NEW_STATE);
        assert_eq!(settlement.block_prove_to, Hash::from_bytes(BLOCK_PROVE_TO));
    }

    #[test]
    fn extracts_tail_from_succinct_layout() {
        let sig_script = build_sig_script(&[
            &[0x01; 32],         // claim
            &0u32.to_le_bytes(), // control_index
            &[0x02; 64],         // control_digests
            &[0x03; 200_000],    // seal (large)
        ]);
        let tx = settlement_tx(sig_script, COVENANT_ID);

        let settlement = tx.settlement_info(COVENANT_ID, CONTAINING_BLOCK).expect("succinct tail");

        assert_eq!(settlement.block_prove_to, Hash::from_bytes(BLOCK_PROVE_TO));
        assert_eq!(settlement.containing_block, CONTAINING_BLOCK);
    }

    #[test]
    fn ignores_tx_without_matching_binding() {
        let sig_script = build_sig_script(&[]);
        let tx = settlement_tx(sig_script, Hash::from_bytes([0x77; 32]));

        assert!(tx.settlement_info(COVENANT_ID, CONTAINING_BLOCK).is_none());
    }

    #[test]
    fn ignores_tx_with_no_outputs_at_all() {
        let tx = Transaction::new(
            TX_VERSION_TOCCATA,
            vec![],
            vec![],
            0,
            SUBNETWORK_ID_NATIVE,
            0,
            Vec::new(),
        );
        assert!(tx.settlement_info(COVENANT_ID, CONTAINING_BLOCK).is_none());
    }

    #[test]
    fn ignores_bootstrap_style_sig_script() {
        // A single short push (e.g. a bare signature) doesn't satisfy the four-push tail.
        let mut sig_script = Vec::new();
        push(&mut sig_script, &[0xDD; 64]);
        let tx = settlement_tx(sig_script, COVENANT_ID);

        assert!(tx.settlement_info(COVENANT_ID, CONTAINING_BLOCK).is_none());
    }

    #[test]
    fn rejects_truncated_push_length_prefix() {
        let mut script = Vec::new();
        push(&mut script, &[0xAB; 16]);
        push(&mut script, &[0xCD; 16]);
        push(&mut script, &[0xEF; 16]);
        script.push(0x4d); // OpPushData2 without its 2-byte length suffix
        assert!(sig_script_tail_4(&script).is_none());
    }

    #[test]
    fn rejects_non_push_opcode() {
        let mut script = build_sig_script(&[]);
        script.push(0x69); // OpVerify - any non-push byte.
        assert!(sig_script_tail_4(&script).is_none());
    }

    #[test]
    fn accepts_pushdata1_and_pushdata2_for_32_byte_items() {
        // Hand-encode the three 32-byte items via OpPushData1 / OpPushData2 instead of OpData32.
        // The parser is opcode-agnostic; it should still extract the payloads.
        let mut script = Vec::new();
        script.push(0x4c);
        script.push(32);
        script.extend_from_slice(&NEW_LANE_TIP);
        script.push(0x4d);
        script.extend_from_slice(&32u16.to_le_bytes());
        script.extend_from_slice(&NEW_STATE);
        script.push(0x4c);
        script.push(32);
        script.extend_from_slice(&BLOCK_PROVE_TO);
        push(&mut script, PREV_REDEEM);

        let tx = settlement_tx(script, COVENANT_ID);
        let settlement =
            tx.settlement_info(COVENANT_ID, CONTAINING_BLOCK).expect("opcode-agnostic tail");
        assert_eq!(settlement.new_lane_tip, Hash::from_bytes(NEW_LANE_TIP));
        assert_eq!(settlement.new_state, NEW_STATE);
        assert_eq!(settlement.block_prove_to, Hash::from_bytes(BLOCK_PROVE_TO));
    }
}
