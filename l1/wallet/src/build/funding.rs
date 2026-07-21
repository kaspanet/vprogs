//! The candidate-prefix funding search: chooses how many leading candidate UTXOs to spend, the
//! fee, and whether the change output survives, so a builder's layout meets its [`FeePolicy`]
//! and stays block-fit.

use std::ops::ControlFlow;

use kaspa_consensus_core::{
    config::params::Params,
    mass::{UtxoCell, calc_storage_mass},
    tx::{Transaction, TransactionOutpoint, UtxoEntry},
};

use super::{
    pricing::{
        DroppedRequirement, FeePolicy, dropped_required_fee, min_fee, priority_mass, required_fee,
    },
    viability::min_viable_change,
};

/// Why a builder could not fund its transaction from the given candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BuildError {
    /// The candidates cannot cover the node's minimum fee (plus a storage-viable change
    /// output where the layout requires one). `available` is what the deepest attempt
    /// brought in beyond the fixed outputs; `required` what it needed. With no candidates
    /// at all, `available` is zero and `required` the zero-prefix layout's floor.
    #[error("candidate UTXOs bring {available} sompi but funding requires {required}")]
    InsufficientFunds { available: u64, required: u64 },
    /// Every funding attempt's fixed outputs exceeded the block-fit storage limit for any
    /// change value. `storage_mass` and `limit` are from one such attempt.
    #[error(
        "fixed outputs alone carry storage mass {storage_mass}, above the block-fit limit {limit}"
    )]
    StorageInfeasible { storage_mass: u64, limit: u64 },
}

/// The fee `tx` pays given the entries its inputs spend: input total minus output total.
pub(super) fn fee_paid(tx: &Transaction, entries: &[UtxoEntry]) -> u64 {
    entries.iter().map(|entry| entry.amount).sum::<u64>()
        - tx.outputs.iter().map(|output| output.value).sum::<u64>()
}

/// How the funding loop decided to pay for one transaction layout.
#[derive(Debug)]
pub(super) struct Funding {
    /// How many leading candidates are spent as funding inputs.
    pub(super) inputs: usize,
    /// Total fee the final transaction pays.
    pub(super) fee: u64,
    /// Change output value, or `None` when the change output is dropped and its remainder
    /// folds into the fee.
    pub(super) change: Option<u64>,
}

/// One pass over the candidate prefixes: the per-prefix funding attempts plus the bookkeeping
/// that decides what [`fund`] returns when no prefix meets `policy`'s requirement.
struct Walk<'a, P> {
    /// Consensus params, for the mass-based fee and storage math.
    params: &'a Params,
    /// How the fee is priced.
    policy: FeePolicy,
    /// The block-fit storage limit every funded layout must stay under.
    limit: u64,
    /// Whether the layout's change output cannot be dropped.
    change_required: bool,
    /// The builder's layout probe (see [`fund`]).
    probe: &'a mut P,
    /// (available, required) from the deepest storage-feasible attempt that still fell short.
    shortfall: Option<(u64, u64)>,
    /// The error from the deepest storage-infeasible attempt, returned only if no attempt ever
    /// recorded a shortfall. Storage feasibility depends on the input set (the KIP-0009 credit
    /// changes with n), so an attempt that is infeasible at one n can still be feasible deeper
    /// in the candidate list, and an early infeasibility must not end the walk.
    infeasible: Option<BuildError>,
    /// Under a target policy, the deepest prefix that could pay its floor (with-change or
    /// dropped-change, whichever this prefix reaches) but not the target rate; returned if the
    /// walk finds no target success (unreachable-target degrade). `n` only grows across the
    /// walk, so each recording is deeper than the last.
    degraded: Option<Funding>,
}

impl<P: FnMut(usize, Option<u64>) -> (Transaction, Vec<UtxoEntry>)> Walk<'_, P> {
    /// Attempts prefix `n`: breaks with its funding when the with-change layout (or, where the
    /// change is droppable, the change-dropped one) meets the policy's requirement; records the
    /// shortfall, degraded candidate, or infeasibility and continues otherwise.
    fn attempt(&mut self, n: usize) -> ControlFlow<Funding> {
        let (tx, entries) = (self.probe)(n, Some(0));
        let entries_total: u64 = entries.iter().map(|entry| entry.amount).sum();
        let outputs_total: u64 = tx.outputs.iter().map(|output| output.value).sum();
        // A short prefix's entries can fall short of the fixed outputs alone; saturate to
        // zero instead of underflowing so the walk keeps adding inputs.
        let available = entries_total.saturating_sub(outputs_total);
        let floor = min_fee(self.params, &tx);

        if available < floor {
            // The with-change layout is unaffordable even at the floor; without the change
            // output the layout shrinks and so does its requirement, which the slack may cover.
            if !self.change_required {
                return self.try_dropped(n, available);
            }
            self.shortfall = Some((available, floor));
            return ControlFlow::Continue(());
        }

        let input_cells: Vec<UtxoCell> = entries.iter().map(UtxoCell::from).collect();
        let output_cells: Vec<UtxoCell> = tx.outputs.iter().map(UtxoCell::from).collect();
        let Some(viable) = min_viable_change(self.params, &input_cells, &output_cells) else {
            let storage = calc_storage_mass(
                false,
                input_cells.iter().copied(),
                output_cells[..output_cells.len() - 1].iter().copied(),
                self.params.storage_mass_parameter,
            )
            .unwrap_or(u64::MAX);
            self.infeasible =
                Some(BuildError::StorageInfeasible { storage_mass: storage, limit: self.limit });
            return ControlFlow::Continue(());
        };
        let cap = available.saturating_sub(viable);
        let params = self.params;
        let pm = |change: u64| priority_mass(params, &tx, &input_cells, &output_cells, change);

        match required_fee(self.policy, floor, available, cap, pm) {
            Ok(fee) => {
                ControlFlow::Break(Funding { inputs: n, fee, change: Some(available - fee) })
            }
            Err(crossing_fee) => {
                if matches!(self.policy, FeePolicy::TargetFeerate(_)) && cap >= floor {
                    self.degraded = Some(Funding { inputs: n, fee: cap, change: Some(viable) });
                }
                // The with-change layout can't reach the requirement; dropping the change
                // only shrinks the layout, so its own (lower) requirement may still fit.
                if !self.change_required {
                    return self.try_dropped(n, available);
                }
                self.shortfall = Some((available, crossing_fee.saturating_add(viable)));
                ControlFlow::Continue(())
            }
        }
    }

    /// Probes prefix `n`'s change-dropped layout: breaks with the whole remainder as the fee
    /// when it meets the policy's requirement and the block-fit storage limit; records the
    /// shortfall, degraded candidate, or infeasibility and continues otherwise. Storage mass is
    /// computed only once `available` covers the layout's own floor: below that, neither a
    /// funded break nor a degraded candidate is reachable, so storage feasibility is
    /// irrelevant, and probing it risks a zero-valued fixed output dividing by zero in
    /// [`calc_storage_mass`] for no benefit.
    fn try_dropped(&mut self, n: usize, available: u64) -> ControlFlow<Funding> {
        let (dropped, dropped_entries) = (self.probe)(n, None);
        let DroppedRequirement { floor, required } =
            dropped_required_fee(self.policy, self.params, &dropped, &dropped_entries);
        if available < floor {
            self.shortfall = Some((available, required));
            return ControlFlow::Continue(());
        }
        let storage_mass = calc_storage_mass(
            false,
            dropped_entries.iter().map(UtxoCell::from),
            dropped.outputs.iter().map(UtxoCell::from),
            self.params.storage_mass_parameter,
        )
        .unwrap_or(u64::MAX);
        if storage_mass > self.limit {
            self.infeasible =
                Some(BuildError::StorageInfeasible { storage_mass, limit: self.limit });
            return ControlFlow::Continue(());
        }
        if available < required {
            if matches!(self.policy, FeePolicy::TargetFeerate(_)) {
                self.degraded = Some(Funding { inputs: n, fee: available, change: None });
            }
            self.shortfall = Some((available, required));
            return ControlFlow::Continue(());
        }
        ControlFlow::Break(Funding { inputs: n, fee: available, change: None })
    }

    /// What the walk resolves to when no prefix met the requirement: the deepest degraded
    /// funding, else the deepest shortfall, else the deepest infeasibility.
    fn conclude(self) -> Result<Funding, BuildError> {
        if let Some(funding) = self.degraded {
            return Ok(funding);
        }
        match self.shortfall {
            Some((available, required)) => {
                Err(BuildError::InsufficientFunds { available, required })
            }
            None => Err(self.infeasible.expect(
                "candidates is non-empty: every attempt records a shortfall or an infeasibility",
            )),
        }
    }
}

/// Chooses how many leading `candidates` to spend, the fee, and whether the change output
/// survives, so the built transaction meets `policy`'s fee requirement and stays block-fit.
///
/// `probe(n, change)` must return the signed layout spending the first `n` candidates (plus
/// any builder-fixed inputs), with the change output last when `change` is `Some` and omitted
/// when `None`, together with the entries all inputs spend, in input order. Probes must not
/// commit storage mass. `change_required` marks layouts whose change output cannot be
/// dropped (it is the transaction's only output). Empty `candidates` return
/// [`BuildError::InsufficientFunds`], not a panic.
pub(super) fn fund(
    params: &Params,
    policy: FeePolicy,
    candidates: &[(TransactionOutpoint, UtxoEntry)],
    change_required: bool,
    probe: &mut impl FnMut(usize, Option<u64>) -> (Transaction, Vec<UtxoEntry>),
) -> Result<Funding, BuildError> {
    if candidates.is_empty() {
        let (tx, _) = probe(0, change_required.then_some(0));
        return Err(BuildError::InsufficientFunds { available: 0, required: min_fee(params, &tx) });
    }
    let mut walk = Walk {
        params,
        policy,
        limit: params.block_mass_limits().raw_post().storage,
        change_required,
        probe,
        shortfall: None,
        infeasible: None,
        degraded: None,
    };
    for n in 1..=candidates.len() {
        if let ControlFlow::Break(funding) = walk.attempt(n) {
            return Ok(funding);
        }
    }
    walk.conclude()
}

/// Regression tests for the funding search's fee/change/input-count decisions under
/// [`FeePolicy::Floor`] and [`FeePolicy::TargetFeerate`]: keeping a viable change, dropping an
/// unviable one, adding inputs when the change is required, the viable-floor boundary, prefix
/// shortfalls, storage infeasibility, and the target-rate fixpoint (byte-dominated,
/// storage-dominated, add-input, drop-change, exact-floor panic guards, and degrade-to-deepest
/// when no candidate set reaches the target).
#[cfg(test)]
mod tests {
    use kaspa_consensus_core::config::params::SIMNET_PARAMS;
    use kaspa_txscript::pay_to_address_script;

    use super::*;
    use crate::build::testing::{
        MEMPOOL_MIN_RELAY_FEE_PER_KILOGRAM, activity_shape, address, candidates_of, keypair,
        mempool_min_fee, payout_probe, priority_mass_mirror, settlement_shape, target_fee_mirror,
    };

    /// One large candidate covers payout, fee, and a viable change: `fund` keeps the change
    /// and prices the fee exactly.
    #[test]
    fn fund_keeps_a_viable_change() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let spk = pay_to_address_script(&address);
        let candidates = candidates_of(&[100_000_000], &address);
        let mut probe = payout_probe(&candidates, 50_000_000, &spk, &keypair);

        let funding =
            fund(params, FeePolicy::Floor, &candidates, false, &mut probe).expect("fundable");
        assert_eq!(funding.inputs, 1);
        let (tx, _) = probe(funding.inputs, Some(0));
        assert_eq!(funding.fee, min_fee(params, &tx));
        let change = funding.change.expect("change survives");
        assert_eq!(change, 100_000_000 - 50_000_000 - funding.fee);
    }

    /// Slack above the fee but below the viable-change floor: the change output is dropped
    /// and the remainder folds into the fee (bounded overpay).
    #[test]
    fn fund_drops_an_unviable_change() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let spk = pay_to_address_script(&address);
        // 400_000 sompi of slack: above the fee, far below the ~2M-sompi viable-change floor.
        let candidates = candidates_of(&[50_000_000 + 400_000], &address);
        let mut probe = payout_probe(&candidates, 50_000_000, &spk, &keypair);

        let funding =
            fund(params, FeePolicy::Floor, &candidates, false, &mut probe).expect("fundable");
        assert_eq!(funding.change, None);
        assert_eq!(funding.fee, 400_000);
        let (tx, _) = probe(funding.inputs, None);
        assert!(funding.fee >= min_fee(params, &tx));
    }

    /// When the layout requires a change output (nothing to drop), `fund` walks down the
    /// candidate list until one more input makes the change viable.
    #[test]
    fn fund_adds_an_input_when_change_is_required() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let spk = pay_to_address_script(&address);
        // First candidate alone leaves an unviable change; the second fixes it.
        let candidates = candidates_of(&[50_000_000 + 400_000, 100_000_000], &address);
        let mut probe = payout_probe(&candidates, 50_000_000, &spk, &keypair);

        let funding =
            fund(params, FeePolicy::Floor, &candidates, true, &mut probe).expect("fundable");
        assert_eq!(funding.inputs, 2);
        assert!(funding.change.is_some());
    }

    /// A change landing exactly on the viable floor is kept, not dropped: the boundary
    /// belongs to the keep side.
    #[test]
    fn fund_keeps_a_change_exactly_at_the_viable_floor() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let spk = pay_to_address_script(&address);
        let payout = 50_000_000;

        // The layout (and so the fee) does not depend on the candidate's amount, but the
        // viable floor does (single-input KIP-0009 credit is C/amount): scout the fee and an
        // initial floor from a throwaway amount, then iterate to the fixed point where
        // building the candidate at `payout + fee + viable` reproduces that same `viable`.
        let scout = candidates_of(&[100_000_000], &address);
        let (tx, entries) = payout_probe(&scout, payout, &spk, &keypair)(1, Some(0));
        let fee = min_fee(params, &tx);
        let mut viable = {
            let inputs: Vec<UtxoCell> = entries.iter().map(UtxoCell::from).collect();
            let outputs: Vec<UtxoCell> = tx.outputs.iter().map(UtxoCell::from).collect();
            min_viable_change(params, &inputs, &outputs).expect("shape is feasible")
        };

        let mut converged = false;
        for _ in 0..20 {
            let amount = payout + fee + viable;
            let candidates = candidates_of(&[amount], &address);
            let (tx, entries) = payout_probe(&candidates, payout, &spk, &keypair)(1, Some(0));
            let inputs: Vec<UtxoCell> = entries.iter().map(UtxoCell::from).collect();
            let outputs: Vec<UtxoCell> = tx.outputs.iter().map(UtxoCell::from).collect();
            let next_viable =
                min_viable_change(params, &inputs, &outputs).expect("shape is feasible");
            if amount == payout + fee + next_viable {
                viable = next_viable;
                converged = true;
                break;
            }
            viable = next_viable;
        }
        assert!(converged, "viable-floor fixed point did not converge within 20 iterations");

        let candidates = candidates_of(&[payout + fee + viable], &address);
        let mut probe = payout_probe(&candidates, payout, &spk, &keypair);
        let funding =
            fund(params, FeePolicy::Floor, &candidates, false, &mut probe).expect("fundable");
        assert_eq!(funding.change, Some(viable));
        assert_eq!(funding.fee, fee);
    }

    /// Candidates that only cover the fixed outputs together: the loop walks past a prefix
    /// whose entries fall short of the payout instead of underflowing.
    #[test]
    fn fund_accumulates_inputs_past_a_payout_shortfall() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let spk = pay_to_address_script(&address);
        // The first candidate alone cannot even cover the payout.
        let candidates = candidates_of(&[60_000_000, 60_000_000], &address);
        let mut probe = payout_probe(&candidates, 100_000_000, &spk, &keypair);

        let funding = fund(params, FeePolicy::Floor, &candidates, false, &mut probe)
            .expect("fundable with both");
        assert_eq!(funding.inputs, 2);
        assert!(funding.change.is_some());
    }

    /// Candidates exhausted below the requirement: a typed insufficiency, not a panic.
    #[test]
    fn fund_reports_insufficient_candidates() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let spk = pay_to_address_script(&address);
        let candidates = candidates_of(&[50_000_000 + 1_000], &address);
        let mut probe = payout_probe(&candidates, 50_000_000, &spk, &keypair);

        let err = fund(params, FeePolicy::Floor, &candidates, true, &mut probe)
            .expect_err("not fundable");
        assert!(matches!(err, BuildError::InsufficientFunds { .. }), "got {err:?}");
    }

    /// No candidates at all: a typed insufficiency carrying the zero-prefix layout's floor,
    /// not a panic, for both the droppable and the change-required layout.
    #[test]
    fn fund_reports_insufficient_funds_for_empty_candidates() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let spk = pay_to_address_script(&address);
        let candidates = candidates_of(&[], &address);
        let mut probe = payout_probe(&candidates, 50_000_000, &spk, &keypair);

        let err = fund(params, FeePolicy::Floor, &candidates, false, &mut probe)
            .expect_err("nothing to fund a droppable layout from");
        let (dropped, _) = probe(0, None);
        let expected =
            BuildError::InsufficientFunds { available: 0, required: min_fee(params, &dropped) };
        assert_eq!(err, expected);

        let err = fund(params, FeePolicy::Floor, &candidates, true, &mut probe)
            .expect_err("nothing to fund a change-required layout from");
        let (with_change, _) = probe(0, Some(0));
        let expected =
            BuildError::InsufficientFunds { available: 0, required: min_fee(params, &with_change) };
        assert_eq!(err, expected);
    }

    /// A fixed output whose storage term alone exceeds the block-fit limit can never be
    /// funded, whatever the change: a typed infeasibility.
    #[test]
    fn fund_reports_storage_infeasible_fixed_outputs() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let spk = pay_to_address_script(&address);
        let candidates = candidates_of(&[100_000_000], &address);
        let mut probe = payout_probe(&candidates, 1_000, &spk, &keypair);

        let err =
            fund(params, FeePolicy::Floor, &candidates, false, &mut probe).expect_err("infeasible");
        assert!(matches!(err, BuildError::StorageInfeasible { .. }), "got {err:?}");
    }

    /// With a storage-dominated priority mass, raising the fee shrinks the change and
    /// raises the mass again: the funding converges on the least fee covering the target
    /// rate over its own final layout, exactly.
    #[test]
    fn target_feerate_prices_a_storage_dominated_layout_exactly() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let spk = pay_to_address_script(&address);
        let candidates = candidates_of(&[100_000_000], &address);
        let mut probe = payout_probe(&candidates, 50_000_000, &spk, &keypair);
        let rate = 500.0;

        let funding = fund(params, FeePolicy::TargetFeerate(rate), &candidates, false, &mut probe)
            .expect("fundable at the target rate");
        let change = funding.change.expect("change survives");
        let (tx, entries) = probe(funding.inputs, Some(change));
        assert_eq!(change, 100_000_000 - 50_000_000 - funding.fee);
        assert_eq!(funding.fee, target_fee_mirror(params, rate, &tx, &entries));
        let priority = priority_mass_mirror(params, &tx, &entries);
        assert!(
            priority > mempool_min_fee(params, &tx) / (MEMPOOL_MIN_RELAY_FEE_PER_KILOGRAM / 1000),
            "storage mass must dominate this shape's priority mass",
        );
        assert!(funding.fee as f64 / priority as f64 >= rate);
    }

    /// When byte mass dominates the priority mass at every reachable change value, the
    /// target rate prices without feedback: fee = ceil(rate x priority mass), exactly.
    #[test]
    fn target_feerate_prices_a_byte_dominated_layout_exactly() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let spk = pay_to_address_script(&address);
        let candidates = candidates_of(&[10_000_000_000], &address);
        let mut probe = payout_probe(&candidates, 5_000_000_000, &spk, &keypair);
        let rate = 250.0;

        let funding = fund(params, FeePolicy::TargetFeerate(rate), &candidates, false, &mut probe)
            .expect("fundable at the target rate");
        let change = funding.change.expect("change survives");
        let (tx, entries) = probe(funding.inputs, Some(change));
        assert_eq!(funding.fee, target_fee_mirror(params, rate, &tx, &entries));
        assert_eq!(
            priority_mass_mirror(params, &tx, &entries),
            mempool_min_fee(params, &tx) / (MEMPOOL_MIN_RELAY_FEE_PER_KILOGRAM / 1000),
            "byte mass must dominate this shape's priority mass",
        );
    }

    /// A single candidate whose change cannot absorb the target-rate fee is walked past:
    /// the second input restores a viable change and the target is met.
    #[test]
    fn target_feerate_adds_an_input_when_the_fee_eats_the_change() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let spk = pay_to_address_script(&address);
        let candidates = candidates_of(&[52_800_000, 100_000_000], &address);
        let mut probe = payout_probe(&candidates, 50_000_000, &spk, &keypair);
        let rate = 200.0;

        let funding = fund(params, FeePolicy::TargetFeerate(rate), &candidates, true, &mut probe)
            .expect("fundable at the target rate with both candidates");
        assert_eq!(funding.inputs, 2);
        let change = funding.change.expect("change survives");
        let (tx, entries) = probe(funding.inputs, Some(change));
        assert_eq!(funding.fee, target_fee_mirror(params, rate, &tx, &entries));
    }

    /// When no viable change can absorb the target-rate fee but the layout can drop its
    /// change, the remainder folds into the fee and the dropped layout still meets the
    /// rate.
    #[test]
    fn target_feerate_drops_an_unviable_change() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let spk = pay_to_address_script(&address);
        let candidates = candidates_of(&[52_500_000], &address);
        let mut probe = payout_probe(&candidates, 50_000_000, &spk, &keypair);
        let rate = 200.0;

        let funding = fund(params, FeePolicy::TargetFeerate(rate), &candidates, false, &mut probe)
            .expect("fundable by dropping the change");
        assert_eq!(funding.change, None);
        assert_eq!(funding.fee, 2_500_000);
        let (tx, entries) = probe(funding.inputs, None);
        assert!(funding.fee >= target_fee_mirror(params, rate, &tx, &entries));
    }

    /// A target-policy prefix whose `available` equals its floor exactly must not probe a
    /// zero-value change: [`calc_storage_mass`] divides by each output's value. With the change
    /// output required, no drop is possible, so this must report a typed insufficiency.
    #[test]
    fn target_feerate_at_exact_floor_availability_does_not_panic_with_change_required() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let spk = pay_to_address_script(&address);
        let payout = 50_000_000;

        // The with-change layout's floor is constant regardless of the exact candidate amount;
        // scout it, then build a candidate that leaves exactly that much available.
        let scout = candidates_of(&[100_000_000], &address);
        let (scout_tx, _) = payout_probe(&scout, payout, &spk, &keypair)(1, Some(0));
        let floor = min_fee(params, &scout_tx);

        let candidates = candidates_of(&[payout + floor], &address);
        let mut probe = payout_probe(&candidates, payout, &spk, &keypair);

        let err = fund(params, FeePolicy::TargetFeerate(200.0), &candidates, true, &mut probe)
            .expect_err("a single candidate at exactly the floor cannot also fund a viable change");
        assert!(matches!(err, BuildError::InsufficientFunds { .. }), "got {err:?}");
    }

    /// The same exact-floor availability as above, but with a droppable change: the fixpoint
    /// still must not probe a zero-value change, and the walk must fall through to dropping the
    /// change instead of panicking.
    #[test]
    fn target_feerate_at_exact_floor_availability_drops_change_instead_of_panicking() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let spk = pay_to_address_script(&address);
        let payout = 50_000_000;

        let scout = candidates_of(&[100_000_000], &address);
        let (scout_tx, _) = payout_probe(&scout, payout, &spk, &keypair)(1, Some(0));
        let floor = min_fee(params, &scout_tx);

        let candidates = candidates_of(&[payout + floor], &address);
        let mut probe = payout_probe(&candidates, payout, &spk, &keypair);

        let funding = fund(params, FeePolicy::TargetFeerate(200.0), &candidates, false, &mut probe)
            .expect("dropping the unreachable change still funds at the floor");
        assert_eq!(funding.change, None);
        assert_eq!(funding.fee, floor);
    }

    /// A droppable layout whose change is unviable even at the floor (so `FeePolicy::Floor`
    /// already drops it) must degrade to that same dropped funding under an unreachable target
    /// rate, not error: the degrade must consider fundings the with-change fixpoint never even
    /// attempts to keep.
    #[test]
    fn unreachable_target_degrades_to_a_dropped_change_funding() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let spk = pay_to_address_script(&address);
        // Same shape as `fund_drops_an_unviable_change`: 400_000 sompi of slack, far below the
        // viable-change floor, so `FeePolicy::Floor` already drops the change here.
        let candidates = candidates_of(&[50_000_000 + 400_000], &address);
        let mut probe = payout_probe(&candidates, 50_000_000, &spk, &keypair);

        let funding =
            fund(params, FeePolicy::TargetFeerate(1_000_000.0), &candidates, false, &mut probe)
                .expect("degrades to the dropped funding instead of erroring");

        let floor_funding =
            fund(params, FeePolicy::Floor, &candidates, false, &mut probe).expect("floor-fundable");
        assert_eq!(funding.inputs, floor_funding.inputs);
        assert_eq!(funding.fee, floor_funding.fee);
        assert_eq!(funding.change, floor_funding.change);
    }

    /// A zero-valued fixed output (which `PayToAddressTx` accepts) below the dropped layout's
    /// floor must not probe storage mass at all: [`calc_storage_mass`] divides by each output's
    /// value, and below the floor neither an `Ok` funding nor a degraded candidate is reachable,
    /// so storage feasibility is irrelevant there. Pre-round-2 `Floor` behavior: a typed
    /// insufficiency, not a panic.
    #[test]
    fn floor_policy_reports_insufficient_funds_for_a_zero_value_output_below_the_dropped_floor() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let spk = pay_to_address_script(&address);
        let payout = 0;

        // The dropped layout's floor depends only on its byte layout, not the candidate amount:
        // scout it from a throwaway input, then fund one sompi short of it.
        let scout = candidates_of(&[100_000_000], &address);
        let (dropped_scout, _) = payout_probe(&scout, payout, &spk, &keypair)(1, None);
        let dropped_floor = min_fee(params, &dropped_scout);

        let candidates = candidates_of(&[dropped_floor - 1], &address);
        let mut probe = payout_probe(&candidates, payout, &spk, &keypair);

        let err = fund(params, FeePolicy::Floor, &candidates, false, &mut probe)
            .expect_err("below the dropped floor, funding must fail, not panic");
        assert!(matches!(err, BuildError::InsufficientFunds { .. }), "got {err:?}");
    }

    /// A degraded dropped funding must be storage-feasible: a droppable fixed-output layout
    /// whose storage mass exceeds the block-fit limit must not be recorded as a degraded
    /// candidate just because it is floor-affordable and the target rate is unreachable there.
    #[test]
    fn unreachable_target_never_degrades_to_a_storage_infeasible_dropped_funding() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let spk = pay_to_address_script(&address);
        // 1_000 sompi payout: C/v is far above the block-fit storage limit, so the dropped
        // layout (this output alone, no change) is storage-infeasible regardless of the fee it
        // pays or the funding input's amount.
        let payout = 1_000;

        // The dropped layout's floor depends only on its byte layout, not the candidate amount:
        // scout it from a throwaway large input, then build a candidate that leaves exactly that
        // much available. That covers what `FeePolicy::Floor` would have paid there (ignoring
        // storage), but stays below the with-change layout's (larger, extra-output) floor, so
        // the walk takes the early unaffordable-with-change branch straight into the
        // dropped-layout probe.
        let scout = candidates_of(&[100_000_000], &address);
        let (dropped_scout, _) = payout_probe(&scout, payout, &spk, &keypair)(1, None);
        let dropped_floor = min_fee(params, &dropped_scout);

        let candidates = candidates_of(&[payout + dropped_floor], &address);
        let mut probe = payout_probe(&candidates, payout, &spk, &keypair);
        let (dropped, dropped_entries) = probe(1, None);
        let dropped_storage = calc_storage_mass(
            false,
            dropped_entries.iter().map(UtxoCell::from),
            dropped.outputs.iter().map(UtxoCell::from),
            params.storage_mass_parameter,
        )
        .unwrap_or(u64::MAX);
        let limit = params.block_mass_limits().raw_post().storage;
        assert!(
            dropped_storage > limit,
            "fixture no longer reproduces a storage-infeasible dropped layout"
        );

        let err =
            fund(params, FeePolicy::TargetFeerate(1_000_000.0), &candidates, false, &mut probe)
                .expect_err("a storage-infeasible dropped layout must not be a degraded funding");
        assert!(
            matches!(err, BuildError::StorageInfeasible { .. }),
            "storage, not target-unreachability, is what blocks this prefix; got {err:?}"
        );
    }

    /// The Floor-policy `fund` outcome (spent inputs, fee, and change) for the keep-change,
    /// drop-change, and add-input shapes, pinned exactly: the estimator coupling must not
    /// perturb any of the three, not just the fee.
    #[test]
    fn floor_policy_funding_outcome_is_pinned_for_every_shape() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let spk = pay_to_address_script(&address);
        let payout = 50_000_000;

        // Keep-change: one large candidate, fee is the mirrored floor, change is the remainder.
        let keep_candidates = candidates_of(&[100_000_000], &address);
        let mut keep_probe = payout_probe(&keep_candidates, payout, &spk, &keypair);
        let keep = fund(params, FeePolicy::Floor, &keep_candidates, false, &mut keep_probe)
            .expect("fundable");
        let (keep_tx, _) = keep_probe(keep.inputs, Some(0));
        let keep_floor = min_fee(params, &keep_tx);
        assert_eq!(keep.inputs, 1);
        assert_eq!(keep.fee, keep_floor);
        assert_eq!(keep.change, Some(100_000_000 - payout - keep_floor));

        // Drop-change: slack above the fee but below the viable-change floor.
        let drop_candidates = candidates_of(&[payout + 400_000], &address);
        let mut drop_probe = payout_probe(&drop_candidates, payout, &spk, &keypair);
        let drop = fund(params, FeePolicy::Floor, &drop_candidates, false, &mut drop_probe)
            .expect("fundable");
        assert_eq!(drop.inputs, 1);
        assert_eq!(drop.fee, 400_000);
        assert_eq!(drop.change, None);

        // Add-input: the first candidate alone leaves an unviable change; change_required forces
        // a second input.
        let add_candidates = candidates_of(&[payout + 400_000, 100_000_000], &address);
        let mut add_probe = payout_probe(&add_candidates, payout, &spk, &keypair);
        let add = fund(params, FeePolicy::Floor, &add_candidates, true, &mut add_probe)
            .expect("fundable");
        let (add_tx, add_entries) = add_probe(add.inputs, Some(0));
        let add_floor = min_fee(params, &add_tx);
        let add_input_cells: Vec<UtxoCell> = add_entries.iter().map(UtxoCell::from).collect();
        let add_output_cells: Vec<UtxoCell> = add_tx.outputs.iter().map(UtxoCell::from).collect();
        let add_viable = min_viable_change(params, &add_input_cells, &add_output_cells)
            .expect("shape is feasible");
        let add_available = payout + 400_000 + 100_000_000 - payout;
        assert_eq!(add.inputs, 2);
        assert_eq!(add.fee, add_floor);
        assert_eq!(add.change, Some(add_available - add_floor));
        assert!(add_available - add_floor >= add_viable, "change must be viable at n=2");
    }

    /// Floor-policy builder shapes that keep their change pay exactly the mempool floor:
    /// the estimator coupling leaves the floor path's fees unchanged.
    #[test]
    fn floor_policy_pays_exactly_the_mempool_floor() {
        let params = &SIMNET_PARAMS;
        for shape in [activity_shape(params), settlement_shape(params)] {
            let paid = fee_paid(&shape.tx, &shape.entries);
            assert_eq!(paid, mempool_min_fee(params, &shape.tx));
        }
    }

    /// A target no candidate set can reach degrades to the deepest affordable funding, not
    /// merely the first one recorded: two floor-affordable prefixes are both individually
    /// viable-with-change, so only checking `funding.change`/`funding.fee` against the
    /// shallowest one would pass even if the deepest-wins overwrite were broken.
    #[test]
    fn unreachable_target_degrades_to_the_deepest_affordable_funding() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let spk = pay_to_address_script(&address);
        let candidates = candidates_of(&[100_000_000, 100_000_000], &address);
        let mut probe = payout_probe(&candidates, 50_000_000, &spk, &keypair);
        let rate = 1_000_000.0;

        let funding = fund(params, FeePolicy::TargetFeerate(rate), &candidates, true, &mut probe)
            .expect("degrades instead of erroring");
        assert_eq!(funding.inputs, candidates.len(), "must fall back to the deepest prefix");

        let (layout, entries) = probe(funding.inputs, Some(0));
        let input_cells: Vec<UtxoCell> = entries.iter().map(UtxoCell::from).collect();
        let output_cells: Vec<UtxoCell> = layout.outputs.iter().map(UtxoCell::from).collect();
        let viable =
            min_viable_change(params, &input_cells, &output_cells).expect("shape is feasible");
        let entries_total: u64 = entries.iter().map(|entry| entry.amount).sum();
        let outputs_total: u64 = layout.outputs.iter().map(|output| output.value).sum();
        let available = entries_total - outputs_total;
        assert_eq!(funding.change, Some(viable));
        assert_eq!(funding.fee, available - viable);
        let (tx, entries) = probe(funding.inputs, Some(viable));
        assert!(funding.fee >= mempool_min_fee(params, &tx));
        assert!(
            (funding.fee as f64 / priority_mass_mirror(params, &tx, &entries) as f64) < rate,
            "the degraded funding stays below the requested rate",
        );
    }

    /// Candidates that cannot even pay the admission floor are a typed insufficiency
    /// under a target policy too, not a degraded funding.
    #[test]
    fn target_feerate_reports_insufficient_candidates() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let spk = pay_to_address_script(&address);
        let candidates = candidates_of(&[50_001_000], &address);
        let mut probe = payout_probe(&candidates, 50_000_000, &spk, &keypair);

        let err =
            fund(params, FeePolicy::TargetFeerate(1_000_000.0), &candidates, true, &mut probe)
                .expect_err("not fundable");
        assert!(matches!(err, BuildError::InsufficientFunds { .. }), "got {err:?}");
    }

    /// A fractional target rate rounds the fee up: the paid rate over the final priority
    /// mass never falls below the requested rate.
    #[test]
    fn target_feerate_rounds_the_fee_up_to_the_node_rate() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let spk = pay_to_address_script(&address);
        let candidates = candidates_of(&[10_000_000_000], &address);
        let mut probe = payout_probe(&candidates, 5_000_000_000, &spk, &keypair);
        let rate = 100.25;

        let funding = fund(params, FeePolicy::TargetFeerate(rate), &candidates, false, &mut probe)
            .expect("fundable at the target rate");
        let change = funding.change.expect("change survives");
        let (tx, entries) = probe(funding.inputs, Some(change));
        assert_eq!(funding.fee, target_fee_mirror(params, rate, &tx, &entries));
        assert_ne!(
            funding.fee,
            mempool_min_fee(params, &tx),
            "the fractional rate must out-bid the floor",
        );
        let priority = priority_mass_mirror(params, &tx, &entries);
        assert!(funding.fee as f64 / priority as f64 >= rate);
    }
}
