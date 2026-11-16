//! Framing identity for vprun's snapshot files.

use vprogs_state_snapshot::SnapshotFormat;

/// Framing identity for vprun's snapshot files: the magic and format version
/// [`SnapshotReader`](vprogs_state_snapshot::SnapshotReader) checks before trusting any framing.
pub struct VpsnapFormat;

impl SnapshotFormat for VpsnapFormat {
    const MAGIC: [u8; 8] = *b"VPSNAP01";
    const FORMAT_VERSION: u16 = 1;
}
