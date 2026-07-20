//! KIP-0009 storage-mass commitment and the block-fit storage limit that bounds a change
//! output's smallest viable value.

use kaspa_consensus_core::{
    config::params::Params,
    constants::MAX_SOMPI,
    mass::{MassCalculator, UtxoCell, calc_storage_mass},
    tx::{PopulatedTransaction, Transaction, UtxoEntry},
};

/// Commits KIP-0009 storage mass on `tx` (Toccata txs must carry it for the node to accept them).
pub fn commit_storage_mass(params: &Params, tx: &Transaction, entries: &[UtxoEntry]) {
    let calc = MassCalculator::new(
        params.mass_per_tx_byte,
        params.mass_per_script_pub_key_byte,
        params.storage_mass_parameter,
    );
    let populated = PopulatedTransaction::new(tx, entries.to_vec());
    let masses = calc
        .calc_contextual_masses(&populated)
        .expect("contextual mass calculation must succeed for a populated transaction");
    tx.set_storage_mass(masses.storage_mass);
}

/// The smallest value the change output can carry while the transaction's KIP-0009 storage
/// mass stays within the block-fit storage limit, the only storage ceiling once Toccata is
/// active (fees cannot buy past it). `inputs` are the final layout's input cells; `outputs`
/// its output cells with the change slot last (that cell's amount is ignored). `None` when no
/// change value fits: the fixed outputs alone already exceed the limit.
pub(super) fn min_viable_change(
    params: &Params,
    inputs: &[UtxoCell],
    outputs: &[UtxoCell],
) -> Option<u64> {
    let limit = params.block_mass_limits().raw_post().storage;
    let (change_slot, fixed) = outputs.split_last()?;
    let storage = |change: u64| {
        let outs = fixed
            .iter()
            .copied()
            .chain(std::iter::once(UtxoCell::new(change_slot.plurality, change)));
        calc_storage_mass(false, inputs.iter().copied(), outs, params.storage_mass_parameter)
            .unwrap_or(u64::MAX)
    };
    // Storage mass is non-increasing in the change value; binary-search the smallest fit.
    (storage(MAX_SOMPI) <= limit).then(|| {
        let (mut lo, mut hi) = (1u64, MAX_SOMPI);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if storage(mid) <= limit { hi = mid } else { lo = mid + 1 }
        }
        lo
    })
}

/// Regression tests for [`min_viable_change`]'s boundary behavior against the block-fit
/// storage limit.
#[cfg(test)]
mod tests {
    use kaspa_consensus_core::config::params::SIMNET_PARAMS;

    use super::*;

    /// [`min_viable_change`] is exact: storage mass fits the block-fit limit at the returned
    /// value and exceeds it one sompi below.
    #[test]
    fn min_viable_change_is_exact_at_the_boundary() {
        let params = &SIMNET_PARAMS;
        let limit = params.block_mass_limits().raw_post().storage;
        let inputs = [UtxoCell::new(1, 100_000_000)];
        let outputs = [UtxoCell::new(1, 50_000_000), UtxoCell::new(1, 0)];
        let storage = |change: u64| {
            let outs = [outputs[0], UtxoCell::new(1, change)];
            calc_storage_mass(
                false,
                inputs.iter().copied(),
                outs.iter().copied(),
                params.storage_mass_parameter,
            )
            .expect("no overflow for these values")
        };

        let mvc = min_viable_change(params, &inputs, &outputs).expect("shape is feasible");
        assert!(
            storage(mvc) <= limit,
            "storage {} at mvc {mvc} exceeds limit {limit}",
            storage(mvc)
        );
        assert!(
            storage(mvc - 1) > limit,
            "storage {} at mvc-1 still fits limit {limit}",
            storage(mvc - 1)
        );
    }

    /// A fixed output so small that its own storage term exceeds the limit is infeasible for
    /// every change value.
    #[test]
    fn min_viable_change_reports_infeasible_fixed_outputs() {
        let params = &SIMNET_PARAMS;
        let inputs = [UtxoCell::new(1, 100_000_000)];
        // 1000 sompi fixed output: C/v = 10^9 grams, far above the 500k block-fit limit.
        let outputs = [UtxoCell::new(1, 1_000), UtxoCell::new(1, 0)];
        assert_eq!(min_viable_change(params, &inputs, &outputs), None);
    }
}
