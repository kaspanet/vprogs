//! Witness serialization for guest consumption.
//!
//! Builds the witness data that gets fed to RISC-0 guest programs as stdin.

use vprogs_node_zk_vm_core::effects::AccessEffect;

use crate::TxEffects;

/// Builds witness data for a sub-proof guest.
///
/// The witness contains the transaction data and pre-state for each
/// accessed resource, so the guest can re-execute and verify.
pub struct WitnessBuilder;

impl WitnessBuilder {
    /// Build witness bytes for a sub-proof guest.
    ///
    /// Layout:
    /// - `tx_index: u32` (4 bytes, LE)
    /// - `num_effects: u32` (4 bytes, LE)
    /// - For each effect:
    ///   - `AccessEffect` (97 bytes)
    /// - `tx_data_len: u32` (4 bytes, LE)
    /// - `tx_data: [u8]`
    /// - For each effect:
    ///   - `pre_state_len: u32` (4 bytes, LE)
    ///   - `pre_state: [u8]`
    pub fn build_sub_proof_witness(
        tx_effects: &TxEffects,
        tx_data: &[u8],
        pre_states: &[Vec<u8>],
    ) -> Vec<u8> {
        assert_eq!(
            tx_effects.effects.len(),
            pre_states.len(),
            "pre_states must match effects count"
        );

        let mut buf = Vec::new();

        // tx_index
        buf.extend_from_slice(&tx_effects.tx_index.to_le_bytes());

        // num_effects
        buf.extend_from_slice(&(tx_effects.effects.len() as u32).to_le_bytes());

        // effects
        for effect in &tx_effects.effects {
            buf.extend_from_slice(&effect.to_bytes());
        }

        // tx_data
        buf.extend_from_slice(&(tx_data.len() as u32).to_le_bytes());
        buf.extend_from_slice(tx_data);

        // pre_states
        for pre_state in pre_states {
            buf.extend_from_slice(&(pre_state.len() as u32).to_le_bytes());
            buf.extend_from_slice(pre_state);
        }

        buf
    }

    /// Build witness bytes for the stitcher guest.
    ///
    /// `context_hashes` are the context_hash values from the sub-proof journals,
    /// produced by the sub-proof guests after proof generation.
    ///
    /// Layout:
    /// - `num_txs: u32` (4 bytes, LE)
    /// - `prev_state_root: [u8; 32]`
    /// - `prev_seq_commitment: [u8; 32]`
    /// - `covenant_id: [u8; 32]`
    /// - For each tx:
    ///   - `sub_journal: [u8; 68]` (SubProofJournal)
    ///   - `num_effects: u32` (4 bytes, LE)
    ///   - For each effect: `AccessEffect` (97 bytes)
    pub fn build_stitcher_witness(
        tx_effects_list: &[TxEffects],
        context_hashes: &[[u8; 32]],
        prev_state_root: [u8; 32],
        prev_seq_commitment: [u8; 32],
        covenant_id: [u8; 32],
    ) -> Vec<u8> {
        use vprogs_node_zk_vm_core::journal::SubProofJournal;

        assert_eq!(
            tx_effects_list.len(),
            context_hashes.len(),
            "context_hashes must match tx_effects_list count"
        );

        let mut buf = Vec::new();

        // num_txs
        buf.extend_from_slice(&(tx_effects_list.len() as u32).to_le_bytes());

        // global params
        buf.extend_from_slice(&prev_state_root);
        buf.extend_from_slice(&prev_seq_commitment);
        buf.extend_from_slice(&covenant_id);

        // per-tx data
        for (tx_effects, ctx_hash) in tx_effects_list.iter().zip(context_hashes.iter()) {
            // sub-proof journal
            let sub_journal = SubProofJournal {
                tx_index: tx_effects.tx_index,
                effects_root: tx_effects.effects_root,
                context_hash: *ctx_hash,
            };
            buf.extend_from_slice(&sub_journal.to_bytes());

            // effects
            buf.extend_from_slice(&(tx_effects.effects.len() as u32).to_le_bytes());
            for effect in &tx_effects.effects {
                buf.extend_from_slice(&effect.to_bytes());
            }
        }

        buf
    }

    /// Parse effects from a stitcher witness for verification.
    #[allow(clippy::type_complexity)]
    pub fn parse_effects_from_witness(
        data: &[u8],
    ) -> Option<Vec<(u32, [u8; 32], Vec<AccessEffect>)>> {
        use vprogs_node_zk_vm_core::journal::SUB_PROOF_JOURNAL_SIZE;

        let mut pos = 0;

        let read_u32 = |pos: &mut usize| -> Option<u32> {
            if *pos + 4 > data.len() {
                return None;
            }
            let val = u32::from_le_bytes(data[*pos..*pos + 4].try_into().unwrap());
            *pos += 4;
            Some(val)
        };

        let read_32 = |pos: &mut usize| -> Option<[u8; 32]> {
            if *pos + 32 > data.len() {
                return None;
            }
            let mut out = [0u8; 32];
            out.copy_from_slice(&data[*pos..*pos + 32]);
            *pos += 32;
            Some(out)
        };

        let num_txs = read_u32(&mut pos)?;
        let _prev_state_root = read_32(&mut pos)?;
        let _prev_seq_commitment = read_32(&mut pos)?;
        let _covenant_id = read_32(&mut pos)?;

        let mut result = Vec::with_capacity(num_txs as usize);

        for _ in 0..num_txs {
            // sub_journal (68 bytes)
            if pos + SUB_PROOF_JOURNAL_SIZE > data.len() {
                return None;
            }
            let sub_journal = vprogs_node_zk_vm_core::journal::SubProofJournal::from_bytes(
                &data[pos..pos + SUB_PROOF_JOURNAL_SIZE],
            )?;
            pos += SUB_PROOF_JOURNAL_SIZE;

            let num_effects = read_u32(&mut pos)?;
            let mut effects = Vec::with_capacity(num_effects as usize);

            for _ in 0..num_effects {
                if pos + 97 > data.len() {
                    return None;
                }
                let bytes: &[u8; 97] = data[pos..pos + 97].try_into().ok()?;
                effects.push(AccessEffect::from_bytes(bytes));
                pos += 97;
            }

            result.push((sub_journal.tx_index, sub_journal.effects_root, effects));
        }

        Some(result)
    }
}
