use alloc::vec::Vec;

use vprogs_core_codec::Reader;
use vprogs_core_smt::proving::Proof;

#[cfg(feature = "host")]
use crate::batch_processor::Bundle;
use crate::{
    Result,
    batch_processor::{Batch, LaneProof},
};

/// Decoded batch processor input (zero-copy).
///
/// Wire layout:
///
/// ```text
/// image_id(32) | covenant_id(32) | lane_key(32)
///   | proof_length(u32 LE) | proof bytes
///   | leaf_order_count(u32 LE) | leaf_order entries (u32 LE each)
///   | num_batches(u32 LE) | batches (Batch wire format, K of them)
///   | lane_proof (LaneProof wire format)
/// ```
///
/// `prev_state` is the bundle-wide SMT root before any batch's writes apply (= `proof.root()`).
/// `new_state` is the bundle-wide SMT root after every batch's writes have been chained
/// through bundle-wide `value_hashes` and re-rooted via `proof.compute_root(...)`.
///
/// `new_seq_commit` derivation uses the *final* batch's `blue_score` together with the
/// bundle-wide `lane_proof` (pre-formed `payload_and_ctx_digest`, `lane_smt_proof`,
/// `parent_seq_commit`) — no per-batch seq-commit derivation.
pub struct Inputs<'a> {
    /// Transaction processor guest image ID used to verify each inner tx journal.
    pub image_id: &'a [u8; 32],
    /// Covenant id the emitted settlement journal binds to.
    pub covenant_id: &'a [u8; 32],
    /// Our lane's key. Bundle-wide (one lane per bundle).
    pub lane_key: &'a [u8; 32],
    /// SMT proof at `v_pre_bundle`, covering union of resources touched across batches.
    pub proof: Proof<'a>,
    /// Bundle-wide leaf-order permutation: `leaf_order[leaf_pos] = bundle_resource_index`.
    pub leaf_order: Vec<u32>,
    /// Batches in scheduling order.
    pub batches: Vec<Batch<'a>>,
    /// Final-block ingredients for the single `new_seq_commit` derivation.
    pub lane_proof: LaneProof<'a>,
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
            batches: buf.many("batches", Batch::decode)?,
            lane_proof: LaneProof::decode(&mut buf)?,
        })
    }

    /// Encodes a bundle input to bytes (host-side).
    ///
    /// Takes the bundle as `&Bundle<S, P>` — owning per-batch data: the `ScheduledBatch`
    /// (which carries `ChainBlockMetadata` via its checkpoint), the host-built
    /// `batch_to_bundle_index` translation, and the per-tx journal byte slices. The kaspa
    /// node's `GetSeqCommitLaneProofResponse` is consumed directly — `LaneProof<'a>`
    /// is the zero-copy decode view of the same fields, so the wire layout for settlement
    /// is symmetric.
    #[cfg(feature = "host")]
    pub fn encode<S, P>(
        image_id: &[u8; 32],
        covenant_id: &[u8; 32],
        lane_key: &[u8; 32],
        proof_bytes: &[u8],
        leaf_order: &[u32],
        bundle: &Bundle<S, P>,
        lane_proof: &kaspa_rpc_core::GetSeqCommitLaneProofResponse,
    ) -> Vec<u8>
    where
        S: vprogs_storage_types::Store,
        P: vprogs_scheduling_scheduler::Processor<
                S,
                BatchMetadata = vprogs_l1_types::ChainBlockMetadata,
            >,
    {
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

        buf.write(&(bundle.len() as u32).to_le_bytes());
        for (batch, batch_to_bundle_index, tx_journals) in bundle.iter() {
            Batch::encode(
                &mut buf,
                batch.checkpoint().metadata(),
                batch_to_bundle_index,
                tx_journals,
            );
        }

        LaneProof::encode(&mut buf, lane_proof);

        buf
    }
}
