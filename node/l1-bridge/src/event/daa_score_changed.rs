/// Virtual DAA score changed.
#[derive(Clone, Debug)]
pub struct DaaScoreChanged {
    /// New virtual DAA score.
    pub daa_score: u64,
}
