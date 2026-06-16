use crate::withdrawal::StandardSpk;

/// Accumulates per-tx exit emissions across a bundle and produces a single 32-byte commitment
/// for the on-chain settlement.
///
/// The [`Verifier`] dispatches every parsed exit through [`add_exit`] in canonical order
/// (journal order within a tx; tx order within a batch; batch order within the bundle). After
/// all batches have been verified, [`finalize`] returns the 32-byte `permission_spk_hash`
/// committed into [`StateTransition`].
///
/// Return [`[0u8; 32]`] from `finalize` when no exits were emitted so settlement stays in
/// single-output mode.
///
/// [`Verifier`]: crate::batch_aggregator::Verifier
/// [`add_exit`]: ExitAccumulator::add_exit
/// [`finalize`]: ExitAccumulator::finalize
/// [`StateTransition`]: crate::batch_aggregator::StateTransition
pub trait ExitAccumulator {
    /// Records a single exit `(destination, amount)`.
    fn add_exit(&mut self, dest: StandardSpk<'_>, amount: u64);

    /// Produces the final 32-byte permission commitment.
    fn finalize(&self) -> [u8; 32];
}
