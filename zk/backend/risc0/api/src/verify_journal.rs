use risc0_zkvm::guest::env;

/// Verifies a composed guest journal against `image_id` via the zkVM's recursive verifier.
pub fn verify_journal(image_id: &[u8; 32], journal: &[u8]) {
    env::verify(*image_id, journal).expect("verify journal");
}
