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
    /// Takes per-section data via [`BatchContext`] which references each section's
    /// `ChainBlockMetadata` directly along with its translation table and tx journals — no
    /// intermediate field-by-field copy. `settlement_*` come pre-formed from the kaspa
    /// node's `get_seq_commit_lane_proof` against the bundle's final block.
    #[cfg(feature = "host")]
    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        image_id: &[u8; 32],
        covenant_id: &[u8; 32],
        lane_key: &[u8; 32],
        proof_bytes: &[u8],
        leaf_order: &[u32],
        sections: &[BatchContext<'_>],
        settlement_payload_and_ctx_digest: &[u8; 32],
        settlement_parent_seq_commit: &[u8; 32],
        settlement_lane_smt_proof: &[u8],
    ) -> Vec<u8> {
        let mut buf = Vec::new();

        buf.extend_from_slice(image_id);
        buf.extend_from_slice(covenant_id);
        buf.extend_from_slice(lane_key);

        buf.extend_from_slice(&(proof_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(proof_bytes);

        buf.extend_from_slice(&(leaf_order.len() as u32).to_le_bytes());
        for &idx in leaf_order {
            buf.extend_from_slice(&idx.to_le_bytes());
        }

        buf.extend_from_slice(&(sections.len() as u32).to_le_bytes());
        for section in sections {
            let m = section.metadata;
            BatchSection::encode(
                &mut buf,
                m.blue_score,
                m.daa_score,
                m.prev_timestamp,
                &m.prev_lane_tip,
                m.lane_blue_score,
                m.lane_expired,
                &m.prev_seq_commit.as_bytes(),
                section.batch_to_bundle_index,
                section.tx_journals,
            );
        }

        SettlementContext::encode(
            &mut buf,
            settlement_payload_and_ctx_digest,
            settlement_parent_seq_commit,
            settlement_lane_smt_proof,
        );

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
