//! Pure L1 transaction construction over a candidate list of funding UTXOs.
//!
//! Every function here is RPC-free: the caller supplies a preference-ordered candidate list of
//! spendable `(outpoint, entry)` pairs, and gets back a signed, finalized transaction whose fee
//! and change the builder chose by spending a prefix of that list (with storage mass committed).
//! [`crate::Wallet`] wraps these with UTXO fetching and submission; an in-process simulation
//! calls them directly with UTXOs it already holds.

use kaspa_addresses::Address;
use kaspa_consensus_core::{
    config::params::Params,
    constants::{MAX_SOMPI, TX_VERSION_TOCCATA},
    hashing::covenant_id::covenant_id,
    mass::{
        ContextualMasses, Mass, MassCalculator, UtxoCell, calc_storage_mass, units::ComputeBudget,
    },
    sign::sign,
    subnets::{SUBNETWORK_ID_NATIVE, SubnetworkId},
    tx::{
        CovenantBinding, MutableTransaction, PopulatedTransaction, Transaction, TransactionInput,
        TransactionOutpoint, TransactionOutput, UtxoEntry,
    },
};
use kaspa_hashes::Hash;
use kaspa_txscript::{pay_to_address_script, standard::pay_to_script_hash_script};
use secp256k1::Keypair;

/// mempool floor: a transaction's fee must be at least this many sompi per mass gram.
const MIN_FEERATE_PER_GRAM: u64 = 100;

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
fn min_fee(params: &Params, tx: &Transaction) -> u64 {
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
fn min_viable_change(params: &Params, inputs: &[UtxoCell], outputs: &[UtxoCell]) -> Option<u64> {
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

/// The node's feerate-ordering mass for a layout ([`Mass::normalized_max`]): the non-contextual
/// masses of `tx` combined with [`calc_storage_mass`] on `input_cells`/`output_cells` (change
/// slot last) at change value `change`. Never commits storage mass; `tx` and the cells come from
/// a zero-value change probe. `change` must be positive: [`calc_storage_mass`] divides by each
/// output's value.
fn priority_mass(
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
fn rate_fee(rate: f64, floor: u64, pm: u64) -> u64 {
    let mut fee = floor.max((rate * pm as f64).ceil() as u64);
    while (fee as f64) / (pm as f64) < rate && fee < u64::MAX {
        fee += 1;
    }
    fee
}

/// A change-dropped layout's fee requirement.
struct DroppedRequirement {
    /// `tx`'s own mempool floor, independent of `policy` (what `FeePolicy::Floor` would pay).
    floor: u64,
    /// `policy`'s requirement: `floor` for [`FeePolicy::Floor`], `rate`-adjusted (never below
    /// `floor`) for [`FeePolicy::TargetFeerate`].
    required: u64,
}

/// `tx`'s requirement under `policy`. `tx` has no change slot, so its priority mass is a fixed
/// value, not a fixpoint.
fn dropped_required_fee(
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
/// it converges (`Ok`, the least sufficient fee) or a step would exceed `cap` (`Err`, carrying
/// the fee that crossed it). 256 steps without convergence is treated as crossing `cap`.
fn required_fee(
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
        if required == fee {
            return Ok(fee);
        }
        fee = required;
    }
    Err(fee)
}

/// Why a builder could not fund its transaction from the given candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildError {
    /// The candidates cannot cover the node's minimum fee (plus a storage-viable change
    /// output where the layout requires one). `available` is what the deepest attempt
    /// brought in beyond the fixed outputs; `required` what it needed.
    InsufficientFunds { available: u64, required: u64 },
    /// Every funding attempt's fixed outputs exceeded the block-fit storage limit for any
    /// change value. `storage_mass` and `limit` are from one such attempt.
    StorageInfeasible { storage_mass: u64, limit: u64 },
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InsufficientFunds { available, required } => {
                write!(f, "candidate UTXOs bring {available} sompi but funding requires {required}")
            }
            Self::StorageInfeasible { storage_mass, limit } => {
                write!(
                    f,
                    "fixed outputs alone carry storage mass {storage_mass}, above the block-fit limit {limit}"
                )
            }
        }
    }
}

impl std::error::Error for BuildError {}

/// The fee `tx` pays given the entries its inputs spend: input total minus output total.
fn fee_paid(tx: &Transaction, entries: &[UtxoEntry]) -> u64 {
    entries.iter().map(|entry| entry.amount).sum::<u64>()
        - tx.outputs.iter().map(|output| output.value).sum::<u64>()
}

/// How the funding loop decided to pay for one transaction layout.
#[derive(Debug)]
struct Funding {
    /// How many leading candidates are spent as funding inputs.
    inputs: usize,
    /// Total fee the final transaction pays.
    fee: u64,
    /// Change output value, or `None` when the change output is dropped and its remainder
    /// folds into the fee.
    change: Option<u64>,
}

/// Outcome of probing prefix `n`'s change-dropped layout (`probe(n, None)`) against `policy`'s
/// requirement and the block-fit storage limit. Storage mass is computed only once `available`
/// covers the layout's own floor: below that, neither an `Ok` funding nor a degraded candidate
/// is reachable, so storage feasibility is irrelevant, and probing it risks a zero-valued fixed
/// output dividing by zero in [`calc_storage_mass`] for no benefit.
enum DroppedProbe {
    /// `available` covers `policy`'s requirement and the layout is storage-feasible: the whole
    /// remainder becomes the fee.
    Funded(Funding),
    /// `available` covers the layout's own floor, but its storage mass exceeds the block-fit
    /// limit for any fee.
    StorageInfeasible { storage_mass: u64 },
    /// `available` covers the layout's own floor and its storage is feasible, but not `policy`'s
    /// (target-level) `required`: eligible as a degraded candidate.
    ShortOfTarget { required: u64 },
    /// `available` falls short of even the layout's own floor: storage was never computed, and
    /// this prefix can never be an `Ok` funding or a degraded candidate.
    Unaffordable { required: u64 },
}

/// Probes prefix `n`'s change-dropped layout and evaluates it against `policy`'s requirement and
/// the block-fit storage limit.
fn try_dropped_funding(
    params: &Params,
    policy: FeePolicy,
    limit: u64,
    n: usize,
    available: u64,
    probe: &mut dyn FnMut(usize, Option<u64>) -> (Transaction, Vec<UtxoEntry>),
) -> DroppedProbe {
    let (dropped, dropped_entries) = probe(n, None);
    let DroppedRequirement { floor, required } =
        dropped_required_fee(policy, params, &dropped, &dropped_entries);
    if available < floor {
        return DroppedProbe::Unaffordable { required };
    }
    let storage_mass = calc_storage_mass(
        false,
        dropped_entries.iter().map(UtxoCell::from),
        dropped.outputs.iter().map(UtxoCell::from),
        params.storage_mass_parameter,
    )
    .unwrap_or(u64::MAX);
    if storage_mass > limit {
        return DroppedProbe::StorageInfeasible { storage_mass };
    }
    if available < required {
        return DroppedProbe::ShortOfTarget { required };
    }
    DroppedProbe::Funded(Funding { inputs: n, fee: available, change: None })
}

/// Chooses how many leading `candidates` to spend, the fee, and whether the change output
/// survives, so the built transaction meets `policy`'s fee requirement and stays block-fit.
///
/// `probe(n, change)` must return the signed layout spending the first `n` candidates (plus
/// any builder-fixed inputs), with the change output last when `change` is `Some` and omitted
/// when `None`, together with the entries all inputs spend, in input order. Probes must not
/// commit storage mass. `change_required` marks layouts whose change output cannot be
/// dropped (it is the transaction's only output).
fn fund(
    params: &Params,
    policy: FeePolicy,
    candidates: &[(TransactionOutpoint, UtxoEntry)],
    change_required: bool,
    probe: &mut dyn FnMut(usize, Option<u64>) -> (Transaction, Vec<UtxoEntry>),
) -> Result<Funding, BuildError> {
    assert!(!candidates.is_empty(), "funding needs at least one candidate UTXO");
    let limit = params.block_mass_limits().raw_post().storage;
    // (available, required) from the deepest storage-feasible attempt that still fell short.
    let mut shortfall: Option<(u64, u64)> = None;
    // The error from the deepest storage-infeasible attempt, returned only if no attempt ever
    // recorded a shortfall. Storage feasibility depends on the input set (the KIP-0009 credit
    // changes with n), so an attempt that is infeasible at one n can still be feasible deeper
    // in the candidate list, and an early infeasibility must not end the search.
    let mut infeasible: Option<BuildError> = None;
    // Under a target policy, the deepest prefix that could pay its floor (with-change or
    // dropped-change, whichever this prefix reaches) but not the target rate; returned if the
    // walk finds no target success (unreachable-target degrade). `n` only grows across the
    // walk, so each recording is deeper than the last.
    let mut degraded: Option<Funding> = None;
    for n in 1..=candidates.len() {
        let (tx, entries) = probe(n, Some(0));
        let entries_total: u64 = entries.iter().map(|entry| entry.amount).sum();
        let outputs_total: u64 = tx.outputs.iter().map(|output| output.value).sum();
        // A short prefix's entries can fall short of the fixed outputs alone; saturate to
        // zero instead of underflowing so the loop keeps adding inputs.
        let available = entries_total.saturating_sub(outputs_total);
        let floor = min_fee(params, &tx);
        let input_cells: Vec<UtxoCell> = entries.iter().map(UtxoCell::from).collect();
        let output_cells: Vec<UtxoCell> = tx.outputs.iter().map(UtxoCell::from).collect();

        if available < floor {
            // The with-change layout is unaffordable even at the floor; without the change
            // output the layout shrinks and so does its requirement, which the slack may cover.
            if !change_required {
                match try_dropped_funding(params, policy, limit, n, available, probe) {
                    DroppedProbe::Funded(funding) => return Ok(funding),
                    DroppedProbe::StorageInfeasible { storage_mass } => {
                        infeasible = Some(BuildError::StorageInfeasible { storage_mass, limit });
                        continue;
                    }
                    DroppedProbe::ShortOfTarget { required } => {
                        if matches!(policy, FeePolicy::TargetFeerate(_)) {
                            degraded = Some(Funding { inputs: n, fee: available, change: None });
                        }
                        shortfall = Some((available, required));
                        continue;
                    }
                    DroppedProbe::Unaffordable { required } => {
                        shortfall = Some((available, required));
                        continue;
                    }
                }
            }
            shortfall = Some((available, floor));
            continue;
        }

        let Some(viable) = min_viable_change(params, &input_cells, &output_cells) else {
            let storage = calc_storage_mass(
                false,
                input_cells.iter().copied(),
                output_cells[..output_cells.len() - 1].iter().copied(),
                params.storage_mass_parameter,
            )
            .unwrap_or(u64::MAX);
            infeasible = Some(BuildError::StorageInfeasible { storage_mass: storage, limit });
            continue;
        };
        let cap = available.saturating_sub(viable);
        let pm = |change: u64| priority_mass(params, &tx, &input_cells, &output_cells, change);

        match required_fee(policy, floor, available, cap, pm) {
            Ok(fee) => return Ok(Funding { inputs: n, fee, change: Some(available - fee) }),
            Err(crossing_fee) => {
                if matches!(policy, FeePolicy::TargetFeerate(_)) && cap >= floor {
                    degraded = Some(Funding { inputs: n, fee: cap, change: Some(viable) });
                }
                if !change_required {
                    // The with-change layout can't reach the requirement; dropping the change
                    // only shrinks the layout, so its own (lower) requirement may still fit.
                    match try_dropped_funding(params, policy, limit, n, available, probe) {
                        DroppedProbe::Funded(funding) => return Ok(funding),
                        DroppedProbe::StorageInfeasible { storage_mass } => {
                            infeasible =
                                Some(BuildError::StorageInfeasible { storage_mass, limit });
                            continue;
                        }
                        DroppedProbe::ShortOfTarget { required } => {
                            if matches!(policy, FeePolicy::TargetFeerate(_)) {
                                degraded =
                                    Some(Funding { inputs: n, fee: available, change: None });
                            }
                            shortfall = Some((available, required));
                            continue;
                        }
                        DroppedProbe::Unaffordable { required } => {
                            shortfall = Some((available, required));
                            continue;
                        }
                    }
                }
                shortfall = Some((available, crossing_fee.saturating_add(viable)));
            }
        }
    }
    if let Some(funding) = degraded {
        return Ok(funding);
    }
    match shortfall {
        Some((available, required)) => Err(BuildError::InsufficientFunds { available, required }),
        None => Err(infeasible.expect(
            "candidates is non-empty: every attempt records a shortfall or an infeasibility",
        )),
    }
}

/// Inputs to [`activity_transaction`].
pub struct ActivityTx<'a> {
    /// Payload carried by the transaction (e.g. an encoded activity instruction).
    pub payload: Vec<u8>,
    /// Funding UTXOs in preference order; the builder spends a prefix of this list.
    pub candidates: Vec<(TransactionOutpoint, UtxoEntry)>,
    /// Key that signs the inputs.
    pub keypair: Keypair,
    /// Address the remainder (after fee) is paid back to.
    pub address: &'a Address,
    /// Subnetwork the tx is issued on; a lane subnetwork for activity, else native.
    pub subnetwork_id: SubnetworkId,
    /// Transaction version; [`TX_VERSION_TOCCATA`] or newer for non-native subnetworks.
    pub tx_version: u16,
    /// How the fee is priced.
    pub fee_policy: FeePolicy,
    /// Consensus params, for the mass-based fee.
    pub params: &'a Params,
}

/// Builds one signed activity transaction funded from a prefix of `args.candidates`, paying
/// the remainder (after the node's minimum mass-based fee) back to `args.address`, carrying
/// `args.payload` on `args.subnetwork_id`.
pub fn activity_transaction(args: ActivityTx<'_>) -> Result<Transaction, BuildError> {
    let spk = pay_to_address_script(args.address);
    let mut build = |n: usize, change: Option<u64>| {
        let inputs = args.candidates[..n]
            .iter()
            .map(|(outpoint, _)| TransactionInput::new(*outpoint, vec![], 0, 1))
            .collect();
        let out_amount = change.expect("the activity output is required");
        let tx = Transaction::new(
            args.tx_version,
            inputs,
            vec![TransactionOutput::new(out_amount, spk.clone())],
            0,
            args.subnetwork_id,
            0,
            args.payload.clone(),
        );
        let entries: Vec<UtxoEntry> =
            args.candidates[..n].iter().map(|(_, entry)| entry.clone()).collect();
        (sign(MutableTransaction::with_entries(tx, entries.clone()), args.keypair).tx, entries)
    };
    let funding = fund(args.params, args.fee_policy, &args.candidates, true, &mut build)?;
    let (tx, entries) = build(funding.inputs, funding.change);
    debug_assert_eq!(fee_paid(&tx, &entries), funding.fee, "tx must pay what `fund` computed");
    commit_storage_mass(args.params, &tx, &entries);
    Ok(tx)
}

/// Inputs to [`pay_to_address_transaction`].
pub struct PayToAddressTx<'a> {
    /// Funding UTXOs in preference order; the builder spends a prefix of this list.
    pub candidates: Vec<(TransactionOutpoint, UtxoEntry)>,
    /// Recipient address each of the `count` outputs pays.
    pub recipient: &'a Address,
    /// Sompi paid to each recipient output.
    pub value: u64,
    /// Number of equal-value outputs to the recipient.
    pub count: usize,
    /// Key that funds the inputs and receives the change.
    pub keypair: Keypair,
    /// Address the remainder (after the recipient outputs and the fee) is paid back to.
    pub change_address: &'a Address,
    /// Consensus params, for the mass-based fee and storage mass.
    pub params: &'a Params,
}

/// Builds one signed transaction paying `args.count` outputs of `args.value` sompi each to
/// `args.recipient`, funded from a prefix of `args.candidates`, returning the remainder
/// (after those outputs and the node's minimum mass-based fee) as change to
/// `args.change_address` when the remainder is storage-viable, and folding it into the fee
/// otherwise. Used to seed a distinct prover's funding address from a coinbase wallet.
pub fn pay_to_address_transaction(args: PayToAddressTx<'_>) -> Result<Transaction, BuildError> {
    let recipient_spk = pay_to_address_script(args.recipient);
    let change_spk = pay_to_address_script(args.change_address);
    args.value.checked_mul(args.count as u64).expect("recipient payout overflow");
    let mut build = |n: usize, change: Option<u64>| {
        let inputs = args.candidates[..n]
            .iter()
            .map(|(outpoint, _)| TransactionInput::new(*outpoint, vec![], 0, 1))
            .collect();
        let mut outputs: Vec<TransactionOutput> = (0..args.count)
            .map(|_| TransactionOutput::new(args.value, recipient_spk.clone()))
            .collect();
        outputs.extend(change.map(|value| TransactionOutput::new(value, change_spk.clone())));
        let tx = Transaction::new(
            TX_VERSION_TOCCATA,
            inputs,
            outputs,
            0,
            SUBNETWORK_ID_NATIVE,
            0,
            Vec::new(),
        );
        let entries: Vec<UtxoEntry> =
            args.candidates[..n].iter().map(|(_, entry)| entry.clone()).collect();
        (sign(MutableTransaction::with_entries(tx, entries.clone()), args.keypair).tx, entries)
    };
    let funding = fund(args.params, FeePolicy::Floor, &args.candidates, false, &mut build)?;
    let (tx, entries) = build(funding.inputs, funding.change);
    commit_storage_mass(args.params, &tx, &entries);
    Ok(tx)
}

/// Builds a signed bootstrap transaction whose single output is P2SH(`redeem_script`) with a
/// genesis covenant binding, funded by `(outpoint, entry)`. Returns the tx and the covenant id
/// consensus will recompute from the input outpoint and output.
pub fn covenant_bootstrap_transaction(
    redeem_script: &[u8],
    value: u64,
    outpoint: TransactionOutpoint,
    entry: UtxoEntry,
    keypair: Keypair,
    params: &Params,
) -> (Transaction, Hash) {
    let covenant_spk = pay_to_script_hash_script(redeem_script);
    let covenant_id = {
        let provisional = TransactionOutput::new(value, covenant_spk.clone());
        covenant_id(outpoint, std::iter::once((0u32, &provisional)))
    };

    let tx_input = TransactionInput::new(outpoint, Vec::new(), 0, 1);
    let tx_output = TransactionOutput::with_covenant(
        value,
        covenant_spk,
        Some(CovenantBinding::new(0, covenant_id)),
    );

    let unsigned = Transaction::new(
        TX_VERSION_TOCCATA,
        vec![tx_input],
        vec![tx_output],
        0,
        SUBNETWORK_ID_NATIVE,
        0,
        Vec::new(),
    );

    let entries = vec![entry];
    let signed = sign(MutableTransaction::with_entries(unsigned, entries.clone()), keypair).tx;
    commit_storage_mass(params, &signed, &entries);

    (signed, covenant_id)
}

/// Inputs to [`settlement_transaction`].
pub struct SettlementTx<'a> {
    /// The covenant-spending settlement tx (its input 0 already carries the covenant witness).
    pub settlement_tx: Transaction,
    /// Entry of the covenant UTXO being spent by input 0.
    pub covenant_entry: UtxoEntry,
    /// Compute budget to set on the covenant input.
    pub covenant_compute_budget: ComputeBudget,
    /// Fee-funding UTXOs in preference order; the builder spends a prefix of this list.
    pub fee_candidates: Vec<(TransactionOutpoint, UtxoEntry)>,
    /// Key that signs the fee inputs.
    pub keypair: Keypair,
    /// Address the change is paid back to.
    pub address: &'a Address,
    /// How the fee is priced.
    pub fee_policy: FeePolicy,
    /// Consensus params, for the mass-based fee and storage mass.
    pub params: &'a Params,
}

/// Funds and signs a settlement transaction without submitting it: appends fee inputs from a
/// prefix of `args.fee_candidates` plus a change output when the remainder is storage-viable
/// (folding it into the fee otherwise), signs the fee inputs, preserves the covenant input's
/// witness, sets the covenant input's compute budget, and commits the storage-mass field.
/// The result is ready to submit.
pub fn settlement_transaction(args: SettlementTx<'_>) -> Result<Transaction, BuildError> {
    // Snapshot the covenant witness before signing - `sign` overwrites all signature scripts.
    let covenant_sig_script = args.settlement_tx.inputs[0].signature_script.clone();
    let change_spk = pay_to_address_script(args.address);
    let mut build = |n: usize, change: Option<u64>| {
        let mut tx = args.settlement_tx.clone();
        tx.inputs.extend(
            args.fee_candidates[..n]
                .iter()
                .map(|(outpoint, _)| TransactionInput::new(*outpoint, vec![], 0, 1)),
        );
        tx.outputs.extend(
            change.map(|value| TransactionOutput::with_covenant(value, change_spk.clone(), None)),
        );
        let entries: Vec<UtxoEntry> = std::iter::once(args.covenant_entry.clone())
            .chain(args.fee_candidates[..n].iter().map(|(_, entry)| entry.clone()))
            .collect();
        let mut tx = sign(MutableTransaction::with_entries(tx, entries.clone()), args.keypair).tx;
        tx.inputs[0].signature_script = covenant_sig_script.clone();
        tx.inputs[0].compute_commit = args.covenant_compute_budget.into();
        (tx, entries)
    };
    let funding = fund(args.params, args.fee_policy, &args.fee_candidates, false, &mut build)?;
    let (mut tx, entries) = build(funding.inputs, funding.change);
    commit_storage_mass(args.params, &tx, &entries);
    // The signature script on input 0 changed, so recompute the on-the-wire id.
    tx.finalize();
    Ok(tx)
}

/// Regression tests for the builders' fee pricing and funding policy.
///
/// Every builder prices its fee from a signed zero-fee probe. The fee floor mirrors the node's
/// post-Toccata admission rule ([`MIN_FEERATE_PER_GRAM`] over the larger of compute mass and
/// normalized transient mass), both of which depend only on the byte layout the fee value
/// cannot change, so the probe prices the final transaction exactly. Storage mass never enters
/// the fee; it bounds validity through the block-fit limit, which the funding loop keeps by
/// requiring a storage-viable change output, dropping the change, or adding inputs.
///
/// The shape tests assert each built transaction pays at least the mirrored node floor
/// ([`mempool_min_fee`]) and stays within the block-fit storage limit.
///
/// Under [`FeePolicy::TargetFeerate`] the fee is raised to the requested rate over the
/// node's priority mass ([`priority_mass_mirror`]), which includes storage mass, so the
/// fee and the change value feed back into each other; the target tests pin the least
/// sufficient fee at convergence, the add-input / drop-change restructuring when a prefix
/// cannot absorb the target fee, and the degrade-to-deepest-affordable rule when no
/// candidate set reaches the target.
#[cfg(test)]
mod tests {
    use kaspa_addresses::{Prefix, Version};
    use kaspa_consensus_core::config::params::SIMNET_PARAMS;

    use super::*;

    /// The value `Wallet::pay_to_address` is asked for when seeding a prover's fee address in
    /// `examples/tn10-flow/tests/two_provers_contend.rs`, i.e. the size of a real fee UTXO.
    const FUND_VALUE: u64 = 100_000_000;

    /// `zk::backend::risc0::covenant::script::DEFAULT_PERMISSION_OUTPUT_VALUE`, the value a real
    /// settlement pays to its permission output.
    const PERMISSION_OUTPUT_VALUE: u64 = 50_000_000;

    /// `sim::driver::l2_driver::DEV_COVENANT_BUDGET`, the budget a dev-mode settlement commits on
    /// its covenant input.
    const DEV_COVENANT_BUDGET: ComputeBudget = ComputeBudget(100);

    fn keypair() -> Keypair {
        Keypair::from_seckey_slice(secp256k1::SECP256K1, &[7u8; 32])
            .expect("static secret key is valid")
    }

    fn address(keypair: &Keypair, params: &Params) -> Address {
        let (xonly, _parity) = keypair.x_only_public_key();
        Address::new(Prefix::from(params.net.network_type()), Version::PubKey, &xonly.serialize())
    }

    fn outpoint(seed: u8) -> TransactionOutpoint {
        TransactionOutpoint::new(Hash::from_bytes([seed; 32]), 0)
    }

    /// The mempool's minimum relay fee, in sompi per 1000 grams of mass, from rusty-kaspa's
    /// post-Toccata `DEFAULT_MINIMUM_RELAY_TRANSACTION_FEE`. Mirrored here because the node's
    /// mempool config is crate-private and this crate does not depend on `kaspa-mining`.
    const MEMPOOL_MIN_RELAY_FEE_PER_KILOGRAM: u64 = 100_000;

    /// The sompi fee a default-configured node's mempool requires for `tx` once Toccata is
    /// active: the relay fee rate over the larger of compute mass and normalized transient
    /// mass. Storage mass never enters the node's fee floor.
    fn mempool_min_fee(params: &Params, tx: &Transaction) -> u64 {
        let calc = MassCalculator::new(
            params.mass_per_tx_byte,
            params.mass_per_script_pub_key_byte,
            params.storage_mass_parameter,
        );
        let masses = calc.calc_non_contextual_masses(tx);
        let cofactors = params.block_mass_limits().raw_post().cofactors();
        let fee_mass = masses.compute_mass.max(masses.normalized_transient(&cofactors));
        (fee_mass * MEMPOOL_MIN_RELAY_FEE_PER_KILOGRAM) / 1000
    }

    /// The node's priority mass for `tx`: the feerate-ordering mass from the mempool's
    /// feerate key, the normalized max of compute, transient, and storage mass. Recomputed
    /// from the cofactors here rather than through the production pricing path, so the two
    /// sides check each other.
    fn priority_mass_mirror(params: &Params, tx: &Transaction, entries: &[UtxoEntry]) -> u64 {
        let calc = MassCalculator::new(
            params.mass_per_tx_byte,
            params.mass_per_script_pub_key_byte,
            params.storage_mass_parameter,
        );
        let masses = calc.calc_non_contextual_masses(tx);
        let populated = PopulatedTransaction::new(tx, entries.to_vec());
        let storage = calc
            .calc_contextual_masses(&populated)
            .expect("contextual mass calculation must succeed for a populated transaction")
            .storage_mass;
        let cofactors = params.block_mass_limits().raw_post().cofactors();
        let storage_normalized = (storage as f64 * cofactors.storage).ceil() as u64;
        masses.compute_mass.max(masses.normalized_transient(&cofactors)).max(storage_normalized)
    }

    /// The least fee paying `rate` sompi per gram of `tx`'s priority mass without dropping
    /// below the mempool floor.
    fn target_fee_mirror(
        params: &Params,
        rate: f64,
        tx: &Transaction,
        entries: &[UtxoEntry],
    ) -> u64 {
        let target = (rate * priority_mass_mirror(params, tx, entries) as f64).ceil() as u64;
        mempool_min_fee(params, tx).max(target)
    }

    /// Fails when `tx` pays less than [`min_fee`] asks for `tx` itself, or when its committed
    /// storage mass is above the block-fit limit.
    ///
    /// The fee check is the builders' own pricing policy, not the node's admission rule; see
    /// [`built_shapes_meet_the_mempool_floor`]. Reports every mass term so a failure shows which
    /// one binds.
    fn assert_fee_covers_final(params: &Params, tx: &Transaction, entries: &[UtxoEntry]) {
        let calc = MassCalculator::new(
            params.mass_per_tx_byte,
            params.mass_per_script_pub_key_byte,
            params.storage_mass_parameter,
        );
        let masses = calc.calc_non_contextual_masses(tx);
        let compute = masses.compute_mass;
        let cofactors = params.block_mass_limits().raw_post().cofactors();
        let transient = masses.normalized_transient(&cofactors);
        let populated = PopulatedTransaction::new(tx, entries.to_vec());
        let storage = calc.calc_contextual_masses(&populated).map_or(0, |m| m.storage_mass);
        let paid = fee_paid(tx, entries);
        let required = min_fee(params, tx);
        assert!(
            paid >= required,
            "built tx pays {paid} but its own min_fee is {required} \
             (compute mass {compute}, normalized transient mass {transient}, storage mass {storage})",
        );
        let limit = params.block_mass_limits().raw_post().storage;
        assert!(
            storage <= limit,
            "built tx carries storage mass {storage}, above the block-fit limit {limit}",
        );
    }

    /// A settlement skeleton with `witness_len` bytes of covenant witness on input 0, a
    /// continuation covenant output, and the permission output. Stands in for what the settler
    /// hands [`settlement_transaction`]; only its size and output values reach the mass
    /// calculation.
    fn settlement_skeleton(
        covenant_spk: &kaspa_consensus_core::tx::ScriptPublicKey,
        covenant_id: Hash,
        continuation_value: u64,
        recipient: &Address,
        witness_len: usize,
    ) -> Transaction {
        let mut tx = Transaction::new(
            TX_VERSION_TOCCATA,
            vec![TransactionInput::new(outpoint(2), vec![0x51u8; witness_len], 0, 1)],
            vec![
                TransactionOutput::with_covenant(
                    continuation_value,
                    covenant_spk.clone(),
                    Some(CovenantBinding::new(0, covenant_id)),
                ),
                TransactionOutput::new(PERMISSION_OUTPUT_VALUE, pay_to_address_script(recipient)),
            ],
            0,
            SUBNETWORK_ID_NATIVE,
            0,
            Vec::new(),
        );
        tx.finalize();
        tx
    }

    /// A transaction a builder produced, with the entries its inputs spend.
    struct BuiltShape {
        tx: Transaction,
        entries: Vec<UtxoEntry>,
    }

    /// A dev-mode settlement, built through [`settlement_transaction`].
    ///
    /// Every value here is one the repo already uses: a `FUND_VALUE` fee UTXO, the default
    /// permission output value, and the dev covenant budget. Only the witness length is a
    /// stand-in for compute mass.
    fn settlement_shape(params: &Params) -> BuiltShape {
        let keypair = keypair();
        let address = address(&keypair, params);
        let covenant_id = Hash::from_bytes([9u8; 32]);
        let covenant_spk = pay_to_script_hash_script(&[0xABu8; 64]);
        let covenant_value = 100_000_000;
        let covenant_entry =
            UtxoEntry::new(covenant_value, covenant_spk.clone(), 0, false, Some(covenant_id));
        let fee_entry = UtxoEntry::new(FUND_VALUE, pay_to_address_script(&address), 0, false, None);

        let tx = settlement_transaction(SettlementTx {
            settlement_tx: settlement_skeleton(
                &covenant_spk,
                covenant_id,
                covenant_value - PERMISSION_OUTPUT_VALUE,
                &address,
                2_000,
            ),
            covenant_entry: covenant_entry.clone(),
            covenant_compute_budget: DEV_COVENANT_BUDGET,
            fee_candidates: vec![(outpoint(3), fee_entry.clone())],
            keypair,
            address: &address,
            fee_policy: FeePolicy::Floor,
            params,
        })
        .expect("settlement is fundable");

        BuiltShape { tx, entries: vec![covenant_entry, fee_entry] }
    }

    /// A payout leaving change small enough that its `C/v` term dominates compute mass, built
    /// through [`pay_to_address_transaction`].
    ///
    /// The change value here is smaller than the coinbase-funded change the repo's own callers
    /// leave, so this shape is reachable through the public builder but is not what `Wallet`
    /// currently produces.
    fn pay_to_address_shape(params: &Params) -> BuiltShape {
        let keypair = keypair();
        let address = address(&keypair, params);
        let payout = FUND_VALUE;
        let change = 50_000_000;
        let entry =
            UtxoEntry::new(payout + change, pay_to_address_script(&address), 0, false, None);

        let tx = pay_to_address_transaction(PayToAddressTx {
            candidates: vec![(outpoint(1), entry.clone())],
            recipient: &address,
            value: payout,
            count: 1,
            keypair,
            change_address: &address,
            params,
        })
        .expect("payout is fundable");

        BuiltShape { tx, entries: vec![entry] }
    }

    /// An activity transaction whose funding UTXO is small enough that the `C/v` term on its single
    /// output dominates compute mass, built through [`activity_transaction`].
    ///
    /// 0.1 KAS is around the smallest output KIP-0009 keeps relayable, and `Wallet` spends its
    /// largest UTXO first, so this shape is reachable through the public builder (which the
    /// simulation calls directly with its own UTXOs) but is not one `Wallet` currently selects.
    fn activity_shape(params: &Params) -> BuiltShape {
        let keypair = keypair();
        let address = address(&keypair, params);
        let entry = UtxoEntry::new(10_000_000, pay_to_address_script(&address), 0, false, None);

        let tx = activity_transaction(ActivityTx {
            payload: vec![0u8; 64],
            candidates: vec![(outpoint(1), entry.clone())],
            keypair,
            address: &address,
            subnetwork_id: SUBNETWORK_ID_NATIVE,
            tx_version: TX_VERSION_TOCCATA,
            fee_policy: FeePolicy::Floor,
            params,
        })
        .expect("activity is fundable");

        BuiltShape { tx, entries: vec![entry] }
    }

    /// A storage-dominated dev-mode settlement, built through [`settlement_transaction`]: its
    /// covenant and permission outputs put storage mass far above compute mass, pinning that
    /// [`min_fee`] still covers the final transaction and stays block-fit even when storage mass
    /// is the larger term.
    #[test]
    fn settlement_fee_covers_the_built_transactions_floor() {
        let params = &SIMNET_PARAMS;
        let shape = settlement_shape(params);
        assert_fee_covers_final(params, &shape.tx, &shape.entries);
    }

    /// A small-change payout, built through [`pay_to_address_transaction`], where the change's
    /// `C/v` term dominates compute mass, pinning that [`min_fee`] still covers the final
    /// transaction and stays block-fit for that shape.
    #[test]
    fn pay_to_address_fee_covers_the_built_transactions_floor() {
        let params = &SIMNET_PARAMS;
        let shape = pay_to_address_shape(params);
        assert_fee_covers_final(params, &shape.tx, &shape.entries);
    }

    /// A small-UTXO activity transaction, built through [`activity_transaction`], where the
    /// single output's `C/v` term dominates compute mass, pinning that [`min_fee`] still covers
    /// the final transaction and stays block-fit for that shape. Also pins that the tx's
    /// committed `storage_mass()` field matches the value
    /// [`MassCalculator::calc_contextual_masses`] recomputes on the populated tx, confirming
    /// [`commit_storage_mass`] took effect.
    #[test]
    fn activity_fee_covers_the_built_transactions_floor() {
        let params = &SIMNET_PARAMS;
        let shape = activity_shape(params);
        assert_fee_covers_final(params, &shape.tx, &shape.entries);

        let calc = MassCalculator::new(
            params.mass_per_tx_byte,
            params.mass_per_script_pub_key_byte,
            params.storage_mass_parameter,
        );
        let populated = PopulatedTransaction::new(&shape.tx, shape.entries.clone());
        let recomputed = calc
            .calc_contextual_masses(&populated)
            .expect("contextual mass calculation must succeed for a populated transaction")
            .storage_mass;
        assert_eq!(
            shape.tx.storage_mass(),
            recomputed,
            "committed storage mass must match the value recomputed on the populated tx",
        );
    }

    /// None of the three shapes above is rejected by a node: [`min_fee`] mirrors the mempool
    /// floor exactly, so every builder's paid fee equals the floor for every shape here.
    ///
    /// [`min_fee`] pays `MIN_FEERATE_PER_GRAM * max(compute, normalized_transient)`, and
    /// [`mempool_min_fee`] mirrors the node's `MEMPOOL_MIN_RELAY_FEE_PER_KILOGRAM / 1000 *
    /// max(compute, normalized_transient)` at the same rate.
    #[test]
    fn built_shapes_meet_the_mempool_floor() {
        let params = &SIMNET_PARAMS;
        for shape in
            [activity_shape(params), pay_to_address_shape(params), settlement_shape(params)]
        {
            let paid = fee_paid(&shape.tx, &shape.entries);
            let floor = mempool_min_fee(params, &shape.tx);
            assert!(paid >= floor, "built tx pays {paid} but the mempool floor is {floor}");
        }
    }

    /// A settlement with a large covenant witness is transient-mass-dominated (2 grams per
    /// byte against compute's ~1): the fee must be priced on normalized transient mass.
    #[test]
    fn witness_heavy_settlement_fee_covers_the_transient_floor() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let covenant_id = Hash::from_bytes([9u8; 32]);
        let covenant_spk = pay_to_script_hash_script(&[0xABu8; 64]);
        let covenant_value = 100_000_000;
        let covenant_entry =
            UtxoEntry::new(covenant_value, covenant_spk.clone(), 0, false, Some(covenant_id));
        let fee_entry = UtxoEntry::new(FUND_VALUE, pay_to_address_script(&address), 0, false, None);

        let tx = settlement_transaction(SettlementTx {
            settlement_tx: settlement_skeleton(
                &covenant_spk,
                covenant_id,
                covenant_value - PERMISSION_OUTPUT_VALUE,
                &address,
                100_000,
            ),
            covenant_entry: covenant_entry.clone(),
            covenant_compute_budget: DEV_COVENANT_BUDGET,
            fee_candidates: vec![(outpoint(3), fee_entry.clone())],
            keypair,
            address: &address,
            fee_policy: FeePolicy::Floor,
            params,
        })
        .expect("settlement is fundable");

        let entries = vec![covenant_entry, fee_entry];
        let paid = fee_paid(&tx, &entries);
        let floor = mempool_min_fee(params, &tx);
        assert!(paid >= floor, "built tx pays {paid} but the node's floor is {floor}");
    }

    /// A payload-heavy activity tx is transient-mass-dominated: normalized transient mass is
    /// 2 grams per serialized byte against compute's ~1, so a large payload puts the node's
    /// fee floor above what compute-only pricing pays.
    #[test]
    fn payload_heavy_activity_fee_covers_the_transient_floor() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let entry = UtxoEntry::new(100_000_000, pay_to_address_script(&address), 0, false, None);

        let tx = activity_transaction(ActivityTx {
            payload: vec![0u8; 10_000],
            candidates: vec![(outpoint(1), entry.clone())],
            keypair,
            address: &address,
            subnetwork_id: SUBNETWORK_ID_NATIVE,
            tx_version: TX_VERSION_TOCCATA,
            fee_policy: FeePolicy::Floor,
            params,
        })
        .expect("activity is fundable");

        let paid = fee_paid(&tx, &[entry]);
        let floor = mempool_min_fee(params, &tx);
        assert!(paid >= floor, "built tx pays {paid} but the node's floor is {floor}");
    }

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

    /// Candidate entries of the given amounts, all P2PK to `address`, with distinct outpoints.
    fn candidates_of(amounts: &[u64], address: &Address) -> Vec<(TransactionOutpoint, UtxoEntry)> {
        amounts
            .iter()
            .enumerate()
            .map(|(i, &amount)| {
                (
                    outpoint(i as u8 + 1),
                    UtxoEntry::new(amount, pay_to_address_script(address), 0, false, None),
                )
            })
            .collect()
    }

    /// A `fund` probe closure for a layout with one fixed `payout` output and an optional
    /// trailing change output, spending the first `n` candidates.
    fn payout_probe<'a>(
        candidates: &'a [(TransactionOutpoint, UtxoEntry)],
        payout: u64,
        spk: &'a kaspa_consensus_core::tx::ScriptPublicKey,
        keypair: &'a Keypair,
    ) -> impl FnMut(usize, Option<u64>) -> (Transaction, Vec<UtxoEntry>) + 'a {
        let keypair = *keypair;
        move |n, change| {
            let inputs = candidates[..n]
                .iter()
                .map(|(o, _)| TransactionInput::new(*o, vec![], 0, 1))
                .collect();
            let mut outputs = vec![TransactionOutput::new(payout, spk.clone())];
            outputs.extend(change.map(|v| TransactionOutput::new(v, spk.clone())));
            let tx = Transaction::new(
                TX_VERSION_TOCCATA,
                inputs,
                outputs,
                0,
                SUBNETWORK_ID_NATIVE,
                0,
                Vec::new(),
            );
            let entries: Vec<UtxoEntry> = candidates[..n].iter().map(|(_, e)| e.clone()).collect();
            (sign(MutableTransaction::with_entries(tx, entries.clone()), keypair).tx, entries)
        }
    }

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

    /// A funding UTXO whose slack above the payout covers the fee but not a storage-viable
    /// change: the change output is dropped and the slack folds into the fee.
    #[test]
    fn pay_to_address_drops_an_unviable_change() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let payout = 50_000_000;
        // 400_000 sompi of slack: above the fee, far below the ~2M viable-change floor.
        let entry =
            UtxoEntry::new(payout + 400_000, pay_to_address_script(&address), 0, false, None);

        let tx = pay_to_address_transaction(PayToAddressTx {
            candidates: vec![(outpoint(1), entry.clone())],
            recipient: &address,
            value: payout,
            count: 1,
            keypair,
            change_address: &address,
            params,
        })
        .expect("fundable without a change output");

        assert_eq!(tx.outputs.len(), 1, "change output must be dropped");
        assert_eq!(fee_paid(&tx, &[entry]), 400_000);
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
