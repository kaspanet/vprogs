/// A single resource's output commitment.
pub enum OutputResourceCommitment<'a> {
    /// Resource data was modified; contains the new data hash.
    Changed(&'a [u8; 32]),
    /// Resource data was not modified.
    Unchanged,
}

impl OutputResourceCommitment<'_> {
    /// Wire flag: resource data was modified (32-byte hash follows).
    pub(crate) const CHANGED: u8 = 0x01;
    /// Wire flag: resource data was not modified (no hash follows).
    pub(crate) const UNCHANGED: u8 = 0x00;
}
