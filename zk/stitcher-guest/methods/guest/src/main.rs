//! Stitcher guest — verifies sub-proofs, checks effects chain, builds SMT.
//!
//! Framework-provided, generic.  Works with ANY sub-proof guest image — it only
//! cares about effects commitments, not the execution logic.
//!
//! ## Input layout (stdin, word-aligned)
//!
//! ```text
//! program_image_id : [u32; 8]
//! num_txs          : u32
//! prev_state_root  : [u32; 8]
//! prev_seq_commit  : [u32; 8]
//! covenant_id      : [u32; 8]
//!
//! For each tx:
//!   sub_journal    : [u32; 17]  (68 bytes — SubProofJournal)
//!   num_effects    : u32
//!   For each effect:
//!     effect_words : [u32; 25]  (100 bytes — 97 bytes of AccessEffect + 3 padding)
//! ```
//!
//! ## Output (journal, 192 bytes = 48 words)
//!
//! See [`StitcherJournal`].

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use risc0_zkvm::{
    guest::env,
    serde::{WordRead, WordWrite},
};
use vprogs_zk_core::{
    effects::AccessEffect,
    journal::SubProofJournal,
    stitcher::{VerifiedTx, run_stitcher},
    words_to_bytes,
};

risc0_zkvm::guest::entry!(main);

pub fn main() {
    let mut stdin = env::stdin();

    // --- Global parameters ---

    let program_image_id = read_hash(&mut stdin);
    let num_txs = read_u32(&mut stdin);
    let prev_state_root = words_to_bytes(read_hash(&mut stdin));
    let prev_seq_commitment = words_to_bytes(read_hash(&mut stdin));
    let covenant_id = words_to_bytes(read_hash(&mut stdin));

    // --- Process each sub-proof ---

    let mut txs = Vec::with_capacity(num_txs as usize);

    for _ in 0..num_txs {
        // Read the sub-proof journal as words, convert to bytes for parsing.
        let mut journal_words = [0u32; 17]; // 68 bytes
        stdin.read_words(&mut journal_words).unwrap();
        let journal_bytes: [u8; 68] = bytemuck::cast(journal_words);

        let sub_journal =
            SubProofJournal::from_bytes(&journal_bytes).expect("invalid sub-proof journal");

        // Verify the sub-proof: the host must have provided a valid receipt
        // for this journal as an assumption.
        env::verify(program_image_id, &journal_bytes).expect("sub-proof verification failed");

        // Read effects (witness data).
        let num_effects = read_u32(&mut stdin);
        let mut effects = Vec::with_capacity(num_effects as usize);

        for _ in 0..num_effects {
            // Each effect is 97 bytes, padded to 100 bytes (25 words) for alignment.
            let mut effect_words = [0u32; 25];
            stdin.read_words(&mut effect_words).unwrap();
            let effect_bytes: [u8; 100] = bytemuck::cast(effect_words);
            effects.push(AccessEffect::from_bytes(
                effect_bytes[..97].try_into().expect("slice length"),
            ));
        }

        txs.push(VerifiedTx { journal: sub_journal, effects });
    }

    // --- Stitcher verification ---

    let program_image_id_bytes = words_to_bytes(program_image_id);
    let journal = run_stitcher(
        &txs,
        prev_state_root,
        prev_seq_commitment,
        covenant_id,
        program_image_id_bytes,
    )
    .expect("stitcher verification failed");

    // --- Write output journal (192 bytes = 48 words) ---

    let journal_bytes = journal.to_bytes();
    let journal_words: [u32; 48] = bytemuck::cast(journal_bytes);
    let mut out = env::journal();
    out.write_words(&journal_words).unwrap();
}

// --- I/O helpers ---

fn read_hash(stdin: &mut impl WordRead) -> [u32; 8] {
    let mut hash = [0u32; 8];
    stdin.read_words(&mut hash).unwrap();
    hash
}

fn read_u32(stdin: &mut impl WordRead) -> u32 {
    let mut val = 0u32;
    stdin.read_words(core::slice::from_mut(&mut val)).unwrap();
    val
}
