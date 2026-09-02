//! Host-side issuer kit over the runtime battery and the L1 wallet: lane-payload
//! assembly, carrier composition, submit retries; the runner stays issuer-free.

pub mod claim;
pub mod genesis;
pub mod lane;
pub mod payload;
pub mod signer;

pub use claim::{
    PermissionSpendArgs, build_permission_spend, claim_siblings, permission_sig_script,
};
pub use genesis::{dev_genesis_keypair, p2pk_address};
pub use lane::{
    CarrierTxArgs, DEFAULT_MAX_SUBMIT_ATTEMPTS, DEFAULT_SUBMIT_RETRY_DELAY,
    covenant_deposit_output, fund_and_submit, is_transient_submit_error, signed_deposit_tx,
    signed_lane_action_tx,
};
pub use payload::{LanePayload, SigRequest};
pub use signer::{
    Bip340Signer, GenesisSchnorrSigPtrSigner, MultisigPrevTxV1WitnessSigner,
    MultisigSchnorrSigPtrSigner, PrevTxV1WitnessSigner, SchnorrSigPtrSigner, SignerKind,
    SignerSpec, TailBlock,
};
