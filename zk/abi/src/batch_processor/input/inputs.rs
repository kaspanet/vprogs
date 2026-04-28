use alloc::vec::Vec;

use vprogs_core_codec::Reader;
use vprogs_core_smt::proving::Proof;

use crate::{
    Result,
    batch_processor::{BatchSection, SettlementContext},
};

/// Decoded batch processor input (zero-copy).
///
/// A bundle proof advances the L2 state from `prev_state` to `new_state` and the lane tip
/// from the first section's `prev_lane_tip` to the last section's derived `new_lane_tip`,
/// across K batches that share a single bundle-wide SMT proof. K=1 is the degenerate single-
/// batch case.
///
/// Wire layout:
///
/// ```text
/// image_id(32) | covenant_id(32) | lane_key(32)
///   | proof_length(u32 LE) | proof bytes
///   | leaf_order_count(u32 LE) | leaf_order entries (u32 LE each)
///   | num_sections(u32 LE) | sections (BatchSection wire format, K of them)
///   | settlement_context (SettlementContext wire format)
/// ```
///
/// `prev_state` is the bundle-wide SMT root before any section's writes apply (= `proof.root()`).
/// `new_state` is the bundle-wide SMT root after every section's writes have been chained
/// through bundle-wide `value_hashes` and re-rooted via `proof.compute_root(...)`.
///
/// `new_seq_commit` derivation uses the *final* section's `blue_score` together with
/// `settlement_context` (pre-formed `payload_and_ctx_digest`, `lane_smt_proof`,
/// `parent_seq_commit`) — no per-section seq-commit derivation.
pub struct Inputs<'a> {
    /// Transaction processor guest image ID used to verify each inner tx journal.
    pub image_id: &'a [u8; 32],
    /// Covenant id the emitted settlement journal binds to.
    pub covenant_id: &'a [u8; 32],
    /// Our lane's key. Bundle-wide (one lane per bundle).
    pub lane_key: &'a [u8; 32],
    /// Bundle-wide L2 state SMT proof at `v_pre_bundle`, covering `union(R_1..R_K)` of
    /// resources touched across all sections.
    pub proof: Proof<'a>,
    /// Bundle-wide leaf-order permutation: `leaf_order[leaf_pos] = bundle_resource_index`.
    /// Length equals `proof.leaves.len()`.
    pub leaf_order: Vec<u32>,
    /// K batch sections in scheduling order.
    pub batches: Vec<BatchSection<'a>>,
    /// Final-block ingredients for the single `new_seq_commit` derivation.
    pub settlement: SettlementContext<'a>,
}

impl<'a> Inputs<'a> {
    /// Decodes the bundle input from a raw byte buffer into zero-copy views.
    pub fn decode(mut buf: &'a [u8]) -> Result<Self> {
        Ok(Self {
            image_id: buf.array::<32>("image_id")?,
            covenant_id: buf.array::<32>("covenant_id")?,
            lane_key: buf.array::<32>("lane_key")?,
            proof: Proof::decode(buf.blob("proof")?)?,
            leaf_order: buf.many("leaf_order", |b| b.le_u32("leaf_order"))?,
            batches: buf.many("batches", BatchSection::decode)?,
            settlement: SettlementContext::decode(&mut buf)?,
        })
    }

    /// Encodes a bundle input to bytes (host-side).
    ///
    /// Takes per-section data via [`BatchContext`] (each section references its
    /// `ChainBlockMetadata` directly) and the kaspa node's
    /// `GetSeqCommitLaneProofResponse` directly — `SettlementContext<'a>` is the zero-copy
    /// decode view of the same fields, so the wire layout for settlement is symmetric.
    #[cfg(feature = "host")]
    pub fn encode(
        image_id: &[u8; 32],
        covenant_id: &[u8; 32],
        lane_key: &[u8; 32],
        proof_bytes: &[u8],
        leaf_order: &[u32],
        sections: &[BatchContext<'_>],
        settlement: &kaspa_rpc_core::GetSeqCommitLaneProofResponse,
    ) -> Vec<u8> {
        use crate::Write;

        let mut buf = Vec::new();
        buf.write(image_id);
        buf.write(covenant_id);
        buf.write(lane_key);
        buf.write(&(proof_bytes.len() as u32).to_le_bytes());
        buf.write(proof_bytes);

        buf.write(&(leaf_order.len() as u32).to_le_bytes());
        for &idx in leaf_order {
            buf.write(&idx.to_le_bytes());
        }

        buf.write(&(sections.len() as u32).to_le_bytes());
        for section in sections {
            BatchSection::encode(
                &mut buf,
                section.metadata,
                section.batch_to_bundle_index,
                section.tx_journals,
            );
        }

        SettlementContext::encode(&mut buf, settlement);

        buf
    }
}

/// Per-batch encode input: the chain-block metadata (read directly), the host-built
/// `batch_to_bundle_index` translation, and the bundle's tx-journal byte slices for this
/// batch's transactions. Mirrors how `transaction_processor::Inputs::encode` consumes a
/// `TransactionContext` directly — no field-by-field copying.
#[cfg(feature = "host")]
pub struct BatchContext<'a> {
    pub metadata: &'a vprogs_l1_types::ChainBlockMetadata,
    pub batch_to_bundle_index: &'a [u32],
    pub tx_journals: &'a [Vec<u8>],
}
