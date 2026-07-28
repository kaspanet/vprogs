//! Snapshot save (and, in a later branch, restore) for bringing up a fresh node past pruning.
pub mod header;
pub mod save;

pub use header::SnapshotHeader;

/// Framing identity for vprun's snapshot files.
pub struct VpsnapFormat;
impl vprogs_state_snapshot::SnapshotFormat for VpsnapFormat {
    const MAGIC: [u8; 8] = *b"VPSNAP01";
    const FORMAT_VERSION: u16 = 1;
}
