//! Configuration for the ZK proving infrastructure.

/// Operating mode for the VM wrapper.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmMode {
    /// Execute and record effects, but do not generate proofs.
    Execute,
    /// Execute, record effects, and generate ZK proofs.
    Prove,
}
