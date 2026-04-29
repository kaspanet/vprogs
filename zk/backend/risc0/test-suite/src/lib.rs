use kaspa_consensus_core::hashing::tx::id as kaspa_tx_id;
use kaspa_hashes::Hash;
use kaspa_seq_commit::{
    hashing::{
        ActivityDigestBuilder, activity_leaf, lane_tip_next, mergeset_context_hash,
        seq_commit_timestamp,
    },
    types::{LaneTipInput, MergesetContext},
};
use vprogs_l1_types::{ChainBlockMetadata, L1Transaction};

/// Loads the pre-built transaction processor ELF from the repository.
pub fn transaction_processor_elf() -> Vec<u8> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let elf_path = format!("{manifest_dir}/../transaction-processor/compiled/program.elf");
    std::fs::read(&elf_path).unwrap_or_else(|e| {
        panic!(
            "transaction processor ELF not found at {elf_path}: {e}\n\
             Run `./zk/backend/risc0/build-guests.sh transaction-processor` to rebuild it."
        )
    })
}

/// Loads the pre-built batch processor ELF from the repository.
pub fn batch_processor_elf() -> Vec<u8> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let elf_path = format!("{manifest_dir}/../batch-processor/compiled/program.elf");
    std::fs::read(&elf_path).unwrap_or_else(|e| {
        panic!(
            "batch processor ELF not found at {elf_path}: {e}\n\
             Run `./zk/backend/risc0/build-guests.sh batch-processor` to rebuild it."
        )
    })
}

/// Computes `lane_tip_next` for a single batch's worth of activity.
///
/// Mirrors the per-section derivation in `vprogs_zk_abi::batch_processor::abi::Abi::verify_section`
/// — used by tests to chain `prev_lane_tip` across bundle sections without depending on the
/// bridge's `advance_lane`. In production the bridge populates `ChainBlockMetadata.lane_tip`
/// per block; tests that build metadata fresh from `metadata_for_block` need to derive the
/// next section's `prev_lane_tip` manually.
pub fn compute_section_lane_tip(
    metadata: &ChainBlockMetadata,
    txs: &[(u32, &L1Transaction)],
    lane_key: &[u8; 32],
) -> [u8; 32] {
    let context_hash = mergeset_context_hash(&MergesetContext {
        timestamp: seq_commit_timestamp(metadata.prev_timestamp),
        daa_score: metadata.daa_score,
        blue_score: metadata.blue_score,
    });

    let mut activity = ActivityDigestBuilder::new();
    for (tx_index, tx) in txs {
        let tx_id = kaspa_tx_id(tx);
        activity.add_leaf(activity_leaf(&tx_id, tx.version, *tx_index));
    }

    let parent_ref = if metadata.lane_expired {
        Hash::from_bytes(metadata.prev_seq_commit.as_bytes())
    } else {
        Hash::from_bytes(metadata.prev_lane_tip)
    };
    let lane_key_h = Hash::from_bytes(*lane_key);

    lane_tip_next(&LaneTipInput {
        parent_ref: &parent_ref,
        lane_key: &lane_key_h,
        activity_digest: &activity.finalize(),
        context_hash: &context_hash,
    })
    .as_bytes()
}
