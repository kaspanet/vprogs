/// Returned by a chain sink that cannot roll back to the requested tip.
///
/// The id is not a block on the sink's canonical chain that it can still roll back to: its block
/// was pruned, reorged away, or never committed. Nothing has changed in the sink.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RollbackRefused;
