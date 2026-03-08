use alloc::vec::Vec;

use vprogs_core_types::ResourceId;

use super::{BatchMetadata, FIXED_HEADER_SIZE, RESOURCE_HEADER_SIZE, Resource};

/// Decodes a wire buffer into zero-copy batch metadata and mutable resource views.
///
/// The buffer is split into an immutable header region and a mutable payload region; each resource
/// receives a disjoint `&mut [u8]` slice into the payload.
pub fn decode(buf: &mut [u8]) -> (&[u8], u32, BatchMetadata<'_>, Vec<Resource<'_>>) {
    let tx_index = u32::from_le_bytes(buf[0..4].try_into().expect("truncated header"));
    let n_resources = u32::from_le_bytes(buf[4..8].try_into().expect("truncated header")) as usize;
    let blue_score = u64::from_le_bytes(buf[40..48].try_into().expect("truncated header"));
    let tx_bytes_len =
        u32::from_le_bytes(buf[48..FIXED_HEADER_SIZE].try_into().expect("truncated header"))
            as usize;

    let tx_bytes_end = FIXED_HEADER_SIZE + tx_bytes_len;
    let resources_header_start = tx_bytes_end;
    let payload_start = resources_header_start + n_resources * RESOURCE_HEADER_SIZE;

    // Split buffer: header part becomes immutable, payload part stays mutable.
    let (header, payload) = buf.split_at_mut(payload_start);
    let header: &[u8] = header;

    // Parse per-resource headers and carve disjoint mutable payload slices in a single pass.
    let mut resources = Vec::with_capacity(n_resources);
    let mut remaining = &mut payload[..];
    for i in 0..n_resources {
        let base = resources_header_start + i * RESOURCE_HEADER_SIZE;
        let rid_bytes: &[u8; 32] = header[base..base + 32].try_into().expect("truncated resource");
        let resource_id = ResourceId::from_bytes_ref(rid_bytes);
        let is_new = header[base + 32] & 1 != 0;
        let resource_index = u32::from_le_bytes(
            header[base + 33..base + 37].try_into().expect("truncated resource"),
        );
        let data_len = u32::from_le_bytes(
            header[base + 37..base + 41].try_into().expect("truncated resource"),
        ) as usize;
        let (slice, rest) = remaining.split_at_mut(data_len);
        remaining = rest;
        resources.push(Resource::new(resource_id, resource_index, is_new, slice));
    }

    let block_hash: &[u8; 32] = header[8..40].try_into().expect("truncated header");
    let tx = &header[FIXED_HEADER_SIZE..tx_bytes_end];

    let batch_metadata = BatchMetadata { block_hash, blue_score };
    (tx, tx_index, batch_metadata, resources)
}
