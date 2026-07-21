//! Pure L1 transaction construction over a candidate list of funding UTXOs.
//!
//! Every function here is RPC-free: the caller supplies a preference-ordered candidate list of
//! spendable `(outpoint, entry)` pairs, and gets back a signed, finalized transaction whose fee
//! and change the builder chose by spending a prefix of that list (with storage mass committed).
//! [`crate::Wallet`] wraps these with UTXO fetching and submission; an in-process simulation
//! calls them directly with UTXOs it already holds.

mod activity;
mod bootstrap;
mod carrier;
mod funding;
mod payout;
mod pricing;
mod settlement;
#[cfg(test)]
mod testing;
mod viability;

pub use activity::{ActivityTx, activity_transaction};
pub use bootstrap::covenant_bootstrap_transaction;
pub use carrier::{SignedCarrierTx, signed_carrier_transaction};
pub use funding::BuildError;
pub use payout::{PayToAddressTx, pay_to_address_transaction};
pub use pricing::FeePolicy;
pub use settlement::{SettlementTx, settlement_transaction};
pub use viability::commit_storage_mass;

/// Cross-builder regression tests that build through more than one of `activity` / `payout` /
/// `settlement` at once.
#[cfg(test)]
mod tests {
    use kaspa_consensus_core::config::params::SIMNET_PARAMS;

    use crate::build::{
        funding::fee_paid,
        testing::{activity_shape, mempool_min_fee, pay_to_address_shape, settlement_shape},
    };

    /// None of the three shapes below is rejected by a node: `min_fee` mirrors the mempool
    /// floor exactly, so every builder's paid fee equals the floor for every shape here.
    ///
    /// `min_fee` pays `MIN_FEERATE_PER_GRAM * max(compute, normalized_transient)`, and
    /// `mempool_min_fee` mirrors the node's `MEMPOOL_MIN_RELAY_FEE_PER_KILOGRAM / 1000 *
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
}
