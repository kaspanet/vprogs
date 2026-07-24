//! Fee policy and the mass-based pricing math builders use to price a signed layout: the
//! mempool admission floor, the priority-mass target rate, and the fixpoint fee search that
//! reconciles a target rate against a change value that shrinks as the fee grows.

use kaspa_consensus_core::{
    config::params::Params,
    mass::{ContextualMasses, Mass, MassCalculator, UtxoCell, calc_storage_mass},
    tx::{Transaction, UtxoEntry},
};

/// mempool floor: a transaction's fee must be at least this many sompi per mass gram.
///
/// This mirrors the node's default minimum relay fee (100_000 sompi per 1000 grams), which is
/// mempool configuration rather than a consensus rule and is reported by no RPC, so a node whose
/// mempool raises it rejects every fee priced on this constant. That is [`FeePolicy::Floor`] and
/// the degraded fundings where [`FeePolicy::TargetFeerate`] cannot reach its target, but never a
/// target rate actually paid: the node clamps its own fee-estimate buckets to its configured
/// minimum, and priority mass is never below the fee-binding mass.
pub(super) const MIN_FEERATE_PER_GRAM: u64 = 100;

/// How a builder prices its fee.
#[derive(Debug, Clone, Copy)]
pub enum FeePolicy {
    /// Pay exactly the node's mempool admission floor.
    Floor,
    /// Target this many sompi per gram of the node's priority mass (which, unlike the
    /// admission floor, includes storage mass), never paying below the floor. The builder
    /// degrades to the closest affordable rate when the candidates cannot reach the target.
    TargetFeerate(f64),
}

/// The minimum sompi fee the node's mempool requires for `tx`: [`MIN_FEERATE_PER_GRAM`] times
/// the fee-binding mass, the larger of the tx's compute mass and its normalized transient
/// mass. Storage mass never enters the node's fee floor; it is bounded separately by the
/// block-fit storage limit.
///
/// Both terms depend only on the serialized byte layout, which the fee value cannot change,
/// so pricing a zero-fee probe of the same layout is exact for the final transaction. Call
/// this on a *signed* layout so the signature scripts are counted (an unsigned tx has empty
/// sig scripts and undercounts both masses).
pub(super) fn min_fee(params: &Params, tx: &Transaction) -> u64 {
    let calc = MassCalculator::new(
        params.mass_per_tx_byte,
        params.mass_per_script_pub_key_byte,
        params.storage_mass_parameter,
    );
    let masses = calc.calc_non_contextual_masses(tx);
    // raw_post: the mempool prices standardness with the post-Toccata cofactors on every
    // network, activation-independent (check_transaction_standard.rs).
    let cofactors = params.block_mass_limits().raw_post().cofactors();
    MIN_FEERATE_PER_GRAM * masses.compute_mass.max(masses.normalized_transient(&cofactors))
}

/// The node's feerate-ordering mass for a layout ([`Mass::normalized_max`]): the non-contextual
/// masses of `tx` combined with [`calc_storage_mass`] on `input_cells`/`output_cells` (change
/// slot last) at change value `change`. Never commits storage mass; `tx` and the cells come from
/// a zero-value change probe. `change` must be positive: [`calc_storage_mass`] divides by each
/// output's value.
pub(super) fn priority_mass(
    params: &Params,
    tx: &Transaction,
    input_cells: &[UtxoCell],
    output_cells: &[UtxoCell],
    change: u64,
) -> u64 {
    debug_assert!(!output_cells.is_empty(), "output_cells must carry the change slot last");
    debug_assert!(change > 0, "change must be positive: calc_storage_mass divides by it");
    let calc = MassCalculator::new(
        params.mass_per_tx_byte,
        params.mass_per_script_pub_key_byte,
        params.storage_mass_parameter,
    );
    let non_contextual = calc.calc_non_contextual_masses(tx);
    let (fixed, change_slot) = output_cells.split_at(output_cells.len() - 1);
    let outputs = fixed
        .iter()
        .copied()
        .chain(std::iter::once(UtxoCell::new(change_slot[0].plurality, change)));
    let storage = calc_storage_mass(
        false,
        input_cells.iter().copied(),
        outputs,
        params.storage_mass_parameter,
    )
    .unwrap_or(u64::MAX);
    let cofactors = params.block_mass_limits().raw_post().cofactors();
    Mass::new(non_contextual, ContextualMasses::new(storage)).normalized_max(&cofactors)
}

/// The least fee that pays at least `rate` sompi per gram of `pm`, never below `floor`, exact
/// against the node's own `fee as f64 / mass as f64 >= rate` comparison: `ceil(rate * pm)` alone
/// can round the f64 product down to an integer that misses that comparison by one or more
/// float rounding steps (the gap between adjacent representable `f64` values grows past
/// `2^53`), so this corrects one sompi at a time until the comparison holds.
pub(super) fn rate_fee(rate: f64, floor: u64, pm: u64) -> u64 {
    let mut fee = floor.max((rate * pm as f64).ceil() as u64);
    while (fee as f64) / (pm as f64) < rate && fee < u64::MAX {
        fee += 1;
    }
    fee
}

/// A change-dropped layout's fee requirement.
pub(super) struct DroppedRequirement {
    /// `tx`'s own mempool floor, independent of `policy` (what `FeePolicy::Floor` would pay).
    pub(super) floor: u64,
    /// `policy`'s requirement: `floor` for [`FeePolicy::Floor`], `rate`-adjusted (never below
    /// `floor`) for [`FeePolicy::TargetFeerate`].
    pub(super) required: u64,
}

/// `tx`'s requirement under `policy`. `tx` has no change slot, so its priority mass is a fixed
/// value, not a fixpoint.
pub(super) fn dropped_required_fee(
    policy: FeePolicy,
    params: &Params,
    tx: &Transaction,
    entries: &[UtxoEntry],
) -> DroppedRequirement {
    let floor = min_fee(params, tx);
    let FeePolicy::TargetFeerate(rate) = policy else {
        return DroppedRequirement { floor, required: floor };
    };
    let calc = MassCalculator::new(
        params.mass_per_tx_byte,
        params.mass_per_script_pub_key_byte,
        params.storage_mass_parameter,
    );
    let non_contextual = calc.calc_non_contextual_masses(tx);
    let storage = calc_storage_mass(
        false,
        entries.iter().map(UtxoCell::from),
        tx.outputs.iter().map(UtxoCell::from),
        params.storage_mass_parameter,
    )
    .unwrap_or(u64::MAX);
    let cofactors = params.block_mass_limits().raw_post().cofactors();
    let pm = Mass::new(non_contextual, ContextualMasses::new(storage)).normalized_max(&cofactors);
    DroppedRequirement { floor, required: rate_fee(rate, floor, pm) }
}

/// Runs the per-prefix fee fixpoint under `policy`, `pm(change)` giving the layout's priority
/// mass at that change value. `cap` (the largest fee keeping the change viable) is rejected
/// before any priority-mass probe: a fixpoint that starts above `cap` can never converge, and
/// probing a change below the viable floor risks a zero-value change, which
/// [`calc_storage_mass`] cannot price. [`FeePolicy::Floor`] evaluates `floor` once.
/// [`FeePolicy::TargetFeerate`] iterates `f' = rate_fee(rate, floor, pm(available - f))` from
/// `f = floor` (monotone non-decreasing, since `pm` is non-increasing in the change value) until
/// a step no longer raises the fee (`Ok`: the least sufficient fee under a monotone `pm`, and in
/// any case a fee covering its own final layout's requirement, so it pays at least the target rate)
/// or a step would exceed `cap` (`Err`, carrying the fee that crossed it). 256 steps without
/// convergence is treated as crossing `cap`.
pub(super) fn required_fee(
    policy: FeePolicy,
    floor: u64,
    available: u64,
    cap: u64,
    pm: impl Fn(u64) -> u64,
) -> Result<u64, u64> {
    if floor > cap {
        return Err(floor);
    }
    let FeePolicy::TargetFeerate(rate) = policy else { return Ok(floor) };
    let mut fee = floor;
    for _ in 0..256 {
        let required = rate_fee(rate, floor, pm(available - fee));
        if required > cap {
            return Err(required);
        }
        if required <= fee {
            return Ok(fee);
        }
        fee = required;
    }
    Err(fee)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ceil(rate * pm)` alone can round the f64 product down to an integer that misses the
    /// node's own `fee as f64 / pm as f64 >= rate` comparison by one float rounding step:
    /// `rate = 100.76515732924022` and `pm = 1303` round to `131297.0`, but
    /// `131297.0 / 1303.0 == 100.76515732924021 < rate`. [`rate_fee`] must correct for this.
    #[test]
    fn rate_fee_never_rounds_below_the_requested_rate_at_an_f64_boundary() {
        let rate = 100.76515732924022;
        let pm = 1303u64;
        let naive = (rate * pm as f64).ceil() as u64;
        assert!(
            (naive as f64 / pm as f64) < rate,
            "test fixture no longer reproduces the boundary"
        );

        let fee = rate_fee(rate, 0, pm);
        assert_eq!(fee, naive + 1, "the single +1 correction must fire for this boundary case");
        assert!(fee as f64 / pm as f64 >= rate, "fee {fee} at pm {pm} rate {rate} falls short");
    }

    /// A non-monotone `pm` (which the storage model should never produce) must not oscillate
    /// the fixpoint into the 256-step bailout: a fee whose next step no longer raises it pays
    /// at least its own layout's requirement and is accepted, merely a shade above the target.
    #[test]
    fn required_fee_accepts_a_fee_a_non_monotone_pm_no_longer_raises() {
        // pm dips once the change drops below 9_850: fee 100 prices to 200, fee 200 prices
        // back to 150, and a strict-equality fixpoint would bounce between them forever.
        let pm = |change: u64| if change >= 9_850 { 200 } else { 150 };
        let fee = required_fee(FeePolicy::TargetFeerate(1.0), 100, 10_000, 5_000, pm)
            .expect("a non-raising step must converge, not bail out");
        assert_eq!(fee, 200);
    }

    /// Above `2^53`, adjacent `u64` values can round to the same `f64`, so a single `+1`
    /// correction is not always enough: `rate = 6_924_528_884_886.104` and `pm = 1303` round to
    /// `9_022_661_137_006_592`, and `9_022_661_137_006_593` (the naive `+1`) still rounds back to
    /// the same `f64` and still falls short; only `9_022_661_137_006_594` clears the comparison.
    /// [`rate_fee`] must keep correcting until it does.
    #[test]
    fn rate_fee_corrects_past_a_single_sompi_above_the_f64_integer_range() {
        let rate = 6_924_528_884_886.104;
        let pm = 1303u64;
        let naive = (rate * pm as f64).ceil() as u64;
        assert!(
            (naive as f64 / pm as f64) < rate,
            "test fixture no longer reproduces the boundary"
        );
        assert!(
            ((naive + 1) as f64 / pm as f64) < rate,
            "test fixture no longer reproduces the single-+1 shortfall"
        );

        let fee = rate_fee(rate, 0, pm);
        assert_eq!(fee, naive + 2, "must correct past the single-sompi rounding plateau");
        assert!(fee as f64 / pm as f64 >= rate, "fee {fee} at pm {pm} rate {rate} falls short");
    }
}
