/// Block lifecycle states.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockState {
    /// Hash known, header not yet downloaded.
    HeaderPending = 0,
    /// Header downloaded, parents known, but index may not be assigned yet.
    HeaderKnown = 1,
    /// Index assigned, ready for content download.
    ContentPending = 2,
    /// Full block content available.
    ContentKnown = 3,
}

impl From<BlockState> for u8 {
    fn from(state: BlockState) -> Self {
        state as u8
    }
}

impl TryFrom<u8> for BlockState {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(BlockState::HeaderPending),
            1 => Ok(BlockState::HeaderKnown),
            2 => Ok(BlockState::ContentPending),
            3 => Ok(BlockState::ContentKnown),
            _ => Err(()),
        }
    }
}
