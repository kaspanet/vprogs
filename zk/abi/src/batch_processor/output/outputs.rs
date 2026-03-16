use crate::Write;

/// Per-resource leaf hash updates streamed from guest to host after batch processing.
///
/// Wire format: `n_resources(u32) + [flag(1) + optional_hash(32)] * n_resources`.
/// Positional: one entry per resource in commitment order.
pub struct Outputs;

impl Outputs {
    /// Wire flag: resource leaf hash was modified (32-byte hash follows).
    const CHANGED: u8 = 0x01;
    /// Wire flag: resource leaf hash was not modified.
    const UNCHANGED: u8 = 0x00;

    /// Encodes per-resource leaf hash updates to the host stream (guest-side).
    ///
    /// Each entry is `Some(hash)` if modified, `None` if unchanged.
    pub fn encode(w: &mut impl Write, leaf_updates: &[Option<[u8; 32]>]) {
        w.write(&(leaf_updates.len() as u32).to_le_bytes());
        for update in leaf_updates {
            match update {
                Some(hash) => {
                    w.write(&[Self::CHANGED]);
                    w.write(hash);
                }
                None => {
                    w.write(&[Self::UNCHANGED]);
                }
            }
        }
    }

    /// Decodes per-resource leaf hash updates from the guest output (host-side).
    ///
    /// Returns `Some(hash)` for modified resources, `None` for unchanged ones.
    #[cfg(feature = "host")]
    pub fn decode(mut buf: &[u8]) -> crate::Result<alloc::vec::Vec<Option<[u8; 32]>>> {
        use vprogs_core_utils::Parser;

        use crate::Error;

        let count = buf.consume_u32("n_resources")? as usize;
        let mut updates = alloc::vec::Vec::with_capacity(count);
        for _ in 0..count {
            match buf.consume_u8("flag")? {
                Self::CHANGED => {
                    updates.push(Some(*buf.consume_array::<32>("hash")?));
                }
                Self::UNCHANGED => {
                    updates.push(None);
                }
                _ => return Err(Error::Decode("invalid batch output flag".into())),
            }
        }
        Ok(updates)
    }
}
