#![no_std]
#![no_main]

use vprogs_zk_risc0_guest_env::{
    commit_journal, compute_input_commitment, compute_output_commitment, parse_witness,
    read_witness,
};
use vprogs_zk_types::Journal;

risc0_zkvm::guest::entry!(main);

fn main() {
    let witness_bytes = read_witness();
    let mut witness = parse_witness(&witness_bytes);

    let input = compute_input_commitment(&mut witness.accounts);
    let output = compute_output_commitment(&[]);

    commit_journal(&Journal { tx_index: witness.tx_index, input, output });
}
