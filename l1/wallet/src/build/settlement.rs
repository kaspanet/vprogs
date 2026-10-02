//! Funds and signs a covenant-spending settlement transaction without submitting it, appending
//! fee inputs from a candidate prefix while preserving the covenant input's witness.

use kaspa_addresses::Address;
use kaspa_consensus_core::{
    config::params::Params,
    mass::units::ComputeBudget,
    sign::sign,
    tx::{
        MutableTransaction, Transaction, TransactionInput, TransactionOutpoint, TransactionOutput,
        UtxoEntry,
    },
};
use kaspa_txscript::pay_to_address_script;
use secp256k1::Keypair;

use super::{
    funding::{BuildError, fund},
    pricing::FeePolicy,
    viability::commit_storage_mass,
};

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

/// Regression tests for [`settlement_transaction`]'s fee pricing on a storage-dominated
/// covenant/permission layout and on a transient-mass-dominated large covenant witness.
#[cfg(test)]
mod tests {
    use kaspa_consensus_core::config::params::SIMNET_PARAMS;
    use kaspa_hashes::Hash;
    use kaspa_txscript::standard::pay_to_script_hash_script;

    use super::*;
    use crate::build::{
        funding::fee_paid,
        testing::{
            DEV_COVENANT_BUDGET, FUND_VALUE, PERMISSION_OUTPUT_VALUE, address,
            assert_fee_covers_final, keypair, mempool_min_fee, outpoint, settlement_shape,
            settlement_skeleton,
        },
    };

    /// A storage-dominated dev-mode settlement, built through [`settlement_transaction`]: its
    /// covenant and permission outputs put storage mass far above compute mass, pinning that
    /// `min_fee` still covers the final transaction and stays block-fit even when storage mass
    /// is the larger term.
    #[test]
    fn settlement_fee_covers_the_built_transactions_floor() {
        let params = &SIMNET_PARAMS;
        let shape = settlement_shape(params);
        assert_fee_covers_final(params, &shape.tx, &shape.entries);
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
                covenant_value,
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

    /// A witness whose transient mass (4 grams per byte) nearly fills the per-tx transient
    /// limit, standing in for the fixed ~224k-byte covenant witness of a production settlement:
    /// only a handful of 66-byte P2PK fee inputs fit below the limit.
    const CAP_WITNESS: usize = 248_000;

    /// The fee-candidate count the cap tests offer; more than any admissible prefix reaches.
    const CAP_CANDIDATES: usize = 40;

    /// The with-change layout at prefix depth `n` over the cap tests' fixtures, mirroring the
    /// probe `settlement_transaction` hands the funding walk (signed fee inputs, the covenant
    /// witness restored, the compute budget committed, a trailing zero-value change output), so
    /// the tests measure the admission boundary on the exact layout the walk admits or rejects.
    fn with_change_layout(
        skeleton: &Transaction,
        covenant_entry: &UtxoEntry,
        candidates: &[(TransactionOutpoint, UtxoEntry)],
        change_spk: &kaspa_consensus_core::tx::ScriptPublicKey,
        keypair: &Keypair,
        n: usize,
    ) -> Transaction {
        let witness = skeleton.inputs[0].signature_script.clone();
        let mut tx = skeleton.clone();
        tx.inputs.extend(
            candidates[..n]
                .iter()
                .map(|(outpoint, _)| TransactionInput::new(*outpoint, vec![], 0, 1)),
        );
        tx.outputs.push(TransactionOutput::with_covenant(0, change_spk.clone(), None));
        let entries: Vec<UtxoEntry> = std::iter::once(covenant_entry.clone())
            .chain(candidates[..n].iter().map(|(_, entry)| entry.clone()))
            .collect();
        let mut signed = sign(MutableTransaction::with_entries(tx, entries), *keypair).tx;
        signed.inputs[0].signature_script = witness;
        signed.inputs[0].compute_commit = DEV_COVENANT_BUDGET.into();
        signed
    }

    /// The non-contextual masses (compute, transient) of a layout, from the same calculator the
    /// admission check prices with.
    fn layout_masses(params: &Params, layout: &Transaction) -> (u64, u64) {
        use kaspa_consensus_core::mass::MassCalculator;

        let calc = MassCalculator::new(
            params.mass_per_tx_byte,
            params.mass_per_script_pub_key_byte,
            params.storage_mass_parameter,
        );
        let masses = calc.calc_non_contextual_masses(layout);
        (masses.compute_mass, masses.transient_mass)
    }

    /// A settlement whose fixed witness nearly fills the per-tx admission limit, funded from many
    /// small UTXOs, crosses the limit once enough inputs are attached: the walk must stop
    /// deepening and surface the typed overflow instead of building a transaction the node
    /// rejects outright (a rejection the settler would treat as unrecoverable). The same witness
    /// funds fine from one large UTXO, pinning the overflow on the input count, not the fixed
    /// layout.
    #[test]
    fn settlement_funding_errors_when_fee_inputs_push_the_layout_past_the_mass_cap() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let change_spk = pay_to_address_script(&address);
        let covenant_id = Hash::from_bytes([9u8; 32]);
        let covenant_spk = pay_to_script_hash_script(&[0xABu8; 64]);
        let covenant_value = 100_000_000;
        let covenant_entry =
            UtxoEntry::new(covenant_value, covenant_spk.clone(), 0, false, Some(covenant_id));
        let skeleton =
            settlement_skeleton(&covenant_spk, covenant_id, covenant_value, &address, CAP_WITNESS);
        let candidates: Vec<_> = (0..CAP_CANDIDATES)
            .map(|i| {
                (
                    outpoint(i as u8 + 1),
                    UtxoEntry::new(3_000_000, change_spk.clone(), 0, false, None),
                )
            })
            .collect();

        let err = settlement_transaction(SettlementTx {
            settlement_tx: skeleton.clone(),
            covenant_entry: covenant_entry.clone(),
            covenant_compute_budget: DEV_COVENANT_BUDGET,
            fee_candidates: candidates,
            keypair,
            address: &address,
            fee_policy: FeePolicy::Floor,
            params,
        })
        .expect_err("the layout cannot fit under the per-tx mass cap");
        let BuildError::MassOverflow { mass, limit } = err else {
            panic!("expected a mass overflow, got {err:?}");
        };
        assert_eq!(limit, params.block_mass_limits.transient, "transient binds this fixture");
        assert!(mass > limit);

        let funded = settlement_transaction(SettlementTx {
            settlement_tx: skeleton,
            covenant_entry,
            covenant_compute_budget: DEV_COVENANT_BUDGET,
            fee_candidates: vec![(
                outpoint(99),
                UtxoEntry::new(200_000_000, change_spk, 0, false, None),
            )],
            keypair,
            address: &address,
            fee_policy: FeePolicy::Floor,
            params,
        })
        .expect("one large UTXO funds the same witness");
        assert_eq!(funded.inputs.len(), 2, "the covenant input plus one fee input");
    }

    /// When affordability is reached exactly at the deepest prefix whose layout still fits the
    /// per-tx admission limit, the walk must return that prefix's funding rather than erroring:
    /// the cap bounds how deep the walk may probe, not whether a fitting prefix can fund. The
    /// boundary is measured with the mirrored probe, so the fixture tracks the params' own mass
    /// constants.
    #[test]
    fn settlement_funding_uses_the_deepest_prefix_that_fits_the_mass_cap() {
        let params = &SIMNET_PARAMS;
        let keypair = keypair();
        let address = address(&keypair, params);
        let change_spk = pay_to_address_script(&address);
        let covenant_id = Hash::from_bytes([9u8; 32]);
        let covenant_spk = pay_to_script_hash_script(&[0xABu8; 64]);
        let covenant_value = 100_000_000;
        let covenant_entry =
            UtxoEntry::new(covenant_value, covenant_spk.clone(), 0, false, Some(covenant_id));
        let skeleton =
            settlement_skeleton(&covenant_spk, covenant_id, covenant_value, &address, CAP_WITNESS);
        let unit: Vec<_> = (0..CAP_CANDIDATES)
            .map(|i| (outpoint(i as u8 + 1), UtxoEntry::new(1, change_spk.clone(), 0, false, None)))
            .collect();

        let (compute_cap, transient_cap) =
            (params.block_mass_limits.compute, params.block_mass_limits.transient);
        let n_max = (1..=CAP_CANDIDATES)
            .take_while(|&n| {
                let (compute, transient) = layout_masses(
                    params,
                    &with_change_layout(
                        &skeleton,
                        &covenant_entry,
                        &unit,
                        &change_spk,
                        &keypair,
                        n,
                    ),
                );
                compute <= compute_cap && transient <= transient_cap
            })
            .count();
        assert!(n_max >= 2, "fixture must leave room for a deepest fitting prefix");
        assert!(n_max < CAP_CANDIDATES, "fixture must leave the cap binding inside the list");

        // Candidate values that make the requirement land exactly at n_max: shallower prefixes
        // fall short of the permission output plus the layout's own floor, n_max covers it.
        let floor_here = mempool_min_fee(
            params,
            &with_change_layout(&skeleton, &covenant_entry, &unit, &change_spk, &keypair, n_max),
        );
        let value = (PERMISSION_OUTPUT_VALUE + floor_here).div_ceil(n_max as u64);
        let candidates: Vec<_> = (0..CAP_CANDIDATES)
            .map(|i| {
                (outpoint(i as u8 + 1), UtxoEntry::new(value, change_spk.clone(), 0, false, None))
            })
            .collect();

        let tx = settlement_transaction(SettlementTx {
            settlement_tx: skeleton,
            covenant_entry,
            covenant_compute_budget: DEV_COVENANT_BUDGET,
            fee_candidates: candidates,
            keypair,
            address: &address,
            fee_policy: FeePolicy::Floor,
            params,
        })
        .expect("the deepest admissible prefix funds the settlement");
        assert_eq!(tx.inputs.len(), n_max + 1, "the covenant input plus the n_max fee inputs");
        assert_eq!(tx.outputs.len(), 2, "the tiny remainder folds into the fee, not a change");
    }
}
