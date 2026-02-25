#![no_std]
#![no_main]

use vprogs_zk_risc0_transaction_processor_abi::{
    access_witness, commit_journal, compute_input_commitment, compute_output_commitment,
    read_witness,
};
use vprogs_zk_types::Journal;

risc0_zkvm::guest::entry!(main);

fn main() {
    let witness_bytes = read_witness();
    let witness = access_witness(&witness_bytes);

    let input = compute_input_commitment(&witness.accounts);
    let output = compute_output_commitment(&witness.accounts, &[]);

    commit_journal(&Journal { tx_index: witness.tx_index.to_native(), input, output });
}
