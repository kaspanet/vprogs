use risc0_zkvm::guest::env;

/// Verifies a composed guest journal against `image_id` via the zkVM's recursive verifier.
///
/// Wired into the ABI verifiers' `verify_*_journal` hook (which is generic over the backend) so a
/// guest verifying a sequence of inner proofs -- the batch processor over tx journals, the
/// aggregator over batch journals -- shares one definition of the `env::verify` glue.
pub fn verify_journal(image_id: &[u8; 32], journal: &[u8]) {
    env::verify(*image_id, journal).expect("verify journal");
}
