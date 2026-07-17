//! Repros for [`batch_processor::Verifier`]'s missing canonicalization of prover-supplied
//! structure.
//!
//! The SMT proof and the tx journals are prover-supplied private data, so their shape is untrusted
//! and the verifier is the only place that can constrain it. Each `verifier_rejects_*` test states
//! a property the verifier must hold and fails against a verifier that does not hold it.
//!
//! `verify_batch_journal` is a no-op closure: these tests exercise the verifier's own shape checks,
//! not `env::verify`.

use std::panic::{AssertUnwindSafe, catch_unwind};

use kaspa_hashes::Hash;
use kaspa_seq_commit::{hashing::mergeset_context_hash, types::MergesetContext};
use tempfile::TempDir;
use vprogs_core_hashing::Sha256;
use vprogs_core_smt::{Commitment, Tree, proving::Proof};
use vprogs_core_types::ResourceId;
use vprogs_storage_rocksdb_store::RocksDbStore;
use vprogs_storage_types::Store;
use vprogs_zk_abi::{
    batch_processor::{BatchTransition, Verifier},
    transaction_processor::{OutputCommitment, OutputResourceCommitment, Transaction},
    withdrawal::StandardSpk,
};
use zerocopy::FromBytes;

/// Alice's resource id; its leading zero bit routes her leaf left of Bob's.
const ALICE_KEY: [u8; 32] = [0x00; 32];
/// Bob's resource id.
const BOB_KEY: [u8; 32] = [0xFF; 32];
/// Value hash of Alice's account while it still funds a withdrawal.
const ALICE_FUNDED: [u8; 32] = [0xAA; 32];
/// Value hash of Alice's account after exactly one withdrawal debits it.
const ALICE_DEBITED: [u8; 32] = [0x11; 32];
/// Value hash of Bob's account; untouched by this batch.
const BOB_VALUE: [u8; 32] = [0xBB; 32];
/// Schnorr pubkey Alice's exits pay out to.
const ALICE_PAYOUT: [u8; 32] = [0x77; 32];
/// Amount each of Alice's two withdrawals pays out.
const WITHDRAW_AMOUNT: u64 = 1_000_000;

/// Tree version the batch proves against.
const VERSION: u64 = 1;
/// Blue score of the batch's chain block.
const BLUE_SCORE: u64 = 100;
/// DAA score of the batch's chain block.
const DAA_SCORE: u64 = 200;
/// Timestamp of the batch's selected parent block.
const PREV_TIMESTAMP: u64 = 300;
/// Blue score at which the lane was last active before this batch.
const PREV_LANE_BLUE_SCORE: u64 = 50;

/// Wire size of a `Membership` entry: tag(1) + leaf index(4).
const MEMBERSHIP_SIZE: usize = 5;
/// Wire size of a `Leaf` entry: depth(2) + key_idx(4) + value_hash(32).
const LEAF_SIZE: usize = 38;
/// Wire size of a `HashedNode` sibling entry: kind(1) + hash(32).
const SIBLING_SIZE: usize = 33;
/// Wire size of a keys-table entry.
const KEY_SIZE: usize = 32;

/// What the verifier settled for one batch.
struct Settled {
    /// `prev_state` the covenant checks against its on-chain state.
    prev_state: [u8; 32],
    /// `new_state` the batch settles to.
    new_state: [u8; 32],
    /// Total amount the batch's exits pay out.
    exits_paid: u64,
    /// Number of exits the batch pays out.
    exit_count: usize,
}

/// A batch of two full-balance withdrawals against Alice, proved with a `keys` table that interns
/// her id twice so the second withdrawal reads a membership slot the first never touched.
///
/// The verifier must reject: only one of the two withdrawals is funded.
#[test]
fn verifier_rejects_two_withdrawals_aliasing_one_account_key() {
    let (_dir, store) = funded_store();
    let honest_proof =
        store.prove(&[ResourceId::from(ALICE_KEY), ResourceId::from(BOB_KEY)], VERSION).unwrap();
    let on_chain_root = store.root(VERSION);

    // The honest proof interns Alice once; the malicious prover appends a second entry for her.
    let evil_proof = with_aliased_first_key(&honest_proof);

    let context_hash = context_hash();
    let withdraw = [(StandardSpk::PubKey(&ALICE_PAYOUT), WITHDRAW_AMOUNT)];
    let tx1 = tx_journal(
        &Hash::from_bytes([0xE1; 32]),
        0,
        &context_hash,
        &[(0, ALICE_KEY, ALICE_FUNDED)],
        &withdraw,
        &[Some(ALICE_DEBITED)],
    );
    let tx2 = tx_journal(
        &Hash::from_bytes([0xE2; 32]),
        1,
        &context_hash,
        &[(2, ALICE_KEY, ALICE_FUNDED)],
        &withdraw,
        &[Some(ALICE_DEBITED)],
    );
    let input_bytes = batch_inputs(&evil_proof, &[tx1, tx2]);

    let Ok(settled) = settle(&input_bytes) else {
        return;
    };

    // Reference roots from the honest proof with Alice's single debit applied.
    let honest = Proof::decode(&honest_proof).unwrap();
    let (honest_prev_root, single_debit_root) = honest
        .compute_roots::<Sha256>(|i| {
            if honest_key(&honest, i) == ALICE_KEY { &ALICE_DEBITED } else { &BOB_VALUE }
        })
        .unwrap();

    panic!(
        "batch settled instead of being rejected\n  \
         exits: {} payouts totalling {} against a balance funding one payout of {}\n  \
         prev_state: {} (on-chain root {}, honest proof {}) -- the alias is invisible to the covenant\n  \
         new_state: {} (root folding a single debit {}) -- one debit reached the root, two were paid",
        settled.exit_count,
        settled.exits_paid,
        WITHDRAW_AMOUNT,
        hex(&settled.prev_state),
        hex(&on_chain_root),
        hex(&honest_prev_root),
        hex(&settled.new_state),
        hex(&single_debit_root),
    );
}

/// The same two withdrawals without the alias: both journal `resource_index = 0`.
///
/// Control for [`verifier_rejects_two_withdrawals_aliasing_one_account_key`]: the per-resource hash
/// chaining does catch an intra-batch double-spend, so the alias is what defeats it.
#[test]
fn two_withdrawals_against_one_membership_index_are_rejected() {
    let (_dir, store) = funded_store();
    let proof =
        store.prove(&[ResourceId::from(ALICE_KEY), ResourceId::from(BOB_KEY)], VERSION).unwrap();

    let context_hash = context_hash();
    let withdraw = [(StandardSpk::PubKey(&ALICE_PAYOUT), WITHDRAW_AMOUNT)];
    let tx1 = tx_journal(
        &Hash::from_bytes([0xE1; 32]),
        0,
        &context_hash,
        &[(0, ALICE_KEY, ALICE_FUNDED)],
        &withdraw,
        &[Some(ALICE_DEBITED)],
    );
    let tx2 = tx_journal(
        &Hash::from_bytes([0xE2; 32]),
        1,
        &context_hash,
        &[(0, ALICE_KEY, ALICE_FUNDED)],
        &withdraw,
        &[Some(ALICE_DEBITED)],
    );

    let Err(err) = settle(&batch_inputs(&proof, &[tx1, tx2])) else {
        panic!("batch settled instead of being rejected");
    };
    assert!(
        err.contains("resource hash mismatch"),
        "expected a resource hash mismatch rejection, got: {err}"
    );
}

/// A batch proved with Bob's key queried before Alice's: the queried keys are not in the canonical
/// ascending order the prover delivers.
#[test]
fn verifier_rejects_unsorted_queried_keys() {
    let (_dir, store) = funded_store();
    let proof =
        store.prove(&[ResourceId::from(BOB_KEY), ResourceId::from(ALICE_KEY)], VERSION).unwrap();

    let Err(err) = settle(&batch_inputs(&proof, &[])) else {
        panic!("batch settled instead of being rejected");
    };
    assert!(
        err.contains("queried keys not strictly ascending"),
        "expected a queried-keys-ascending rejection, got: {err}"
    );
}

/// A success journal committing more output resources than it declared input resources.
///
/// The verifier must reject: the journal's output commitments must be one-to-one with its inputs.
#[test]
fn verifier_rejects_a_success_journal_with_more_outputs_than_input_resources() {
    let (_dir, store) = funded_store();
    let proof =
        store.prove(&[ResourceId::from(ALICE_KEY), ResourceId::from(BOB_KEY)], VERSION).unwrap();

    let context_hash = context_hash();
    let tx = tx_journal(
        &Hash::from_bytes([0xE1; 32]),
        0,
        &context_hash,
        &[(0, ALICE_KEY, ALICE_FUNDED)],
        &[],
        // One declared input resource, three output commitments.
        &[Some(ALICE_DEBITED), Some([0xCC; 32]), None],
    );
    let input_bytes = batch_inputs(&proof, &[tx]);

    if settle(&input_bytes).is_err() {
        return;
    }

    panic!("batch settled: the journal's two unconsumed output-resource commitments were ignored");
}

/// Opens a store holding Alice's funded account and Bob's account at [`VERSION`].
fn funded_store() -> (TempDir, RocksDbStore) {
    let dir = TempDir::new().unwrap();
    let store: RocksDbStore = RocksDbStore::open(dir.path());
    let mut wb = store.write_batch();
    store.update(
        &mut wb,
        vec![
            Commitment::new(ResourceId::from(ALICE_KEY), ALICE_FUNDED),
            Commitment::new(ResourceId::from(BOB_KEY), BOB_VALUE),
        ],
        VERSION,
    );
    store.commit(wb);
    (dir, store)
}

/// Runs the batch through the real verifier and returns what it settled, or Err with the panic
/// message if the verifier rejects the batch.
fn settle(input_bytes: &[u8]) -> Result<Settled, String> {
    catch_unwind(AssertUnwindSafe(|| {
        let mut verifier = Verifier::new(input_bytes, |_image_id: &[u8; 32], _journal: &[u8]| {});
        let (new_lane_tip, new_lane_blue_score) = verifier.verify_batch();

        let mut journal = Vec::new();
        verifier.commit_batch_transition::<Sha256>(
            &mut journal,
            &new_lane_tip,
            new_lane_blue_score,
        );
        let transition = BatchTransition::ref_from_bytes(&journal).unwrap();
        let exits: Vec<_> = transition.exits.iter().map(|e| e.unwrap()).collect();

        Settled {
            prev_state: transition.prev_state,
            new_state: transition.new_state,
            exits_paid: exits.iter().map(|&(_, amount)| amount).sum(),
            exit_count: exits.len(),
        }
    }))
    .map_err(|payload| {
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&'static str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "non-string panic payload".to_string())
    })
}

/// Re-encodes `proof` with `keys[0]` appended to the keys table and `memberships[0]` appended to
/// the memberships table, aliasing one key across two membership indices.
///
/// Neither table participates in the root, so the re-encoded proof still proves the same tree.
fn with_aliased_first_key(proof: &[u8]) -> Vec<u8> {
    let mut buf = proof;
    let (n_keys, keys) = section(&mut buf, KEY_SIZE);
    let (n_leaves, leaves) = section(&mut buf, LEAF_SIZE);
    let (n_siblings, siblings) = section(&mut buf, SIBLING_SIZE);
    let (n_topology, topology) = section(&mut buf, 1);
    let (n_memberships, memberships) = section(&mut buf, MEMBERSHIP_SIZE);
    assert!(buf.is_empty(), "proof wire has trailing bytes");

    let mut out = Vec::new();
    write_section(&mut out, n_keys + 1, &[keys, &keys[..KEY_SIZE]].concat());
    write_section(&mut out, n_leaves, leaves);
    write_section(&mut out, n_siblings, siblings);
    write_section(&mut out, n_topology, topology);
    write_section(
        &mut out,
        n_memberships + 1,
        &[memberships, &memberships[..MEMBERSHIP_SIZE]].concat(),
    );
    out
}

/// Reads one `count(4 LE) + count * elem_size` proof section, advancing `buf`.
fn section<'a>(buf: &mut &'a [u8], elem_size: usize) -> (u32, &'a [u8]) {
    let count = u32::from_le_bytes(buf[..4].try_into().unwrap());
    let (bytes, rest) = buf[4..].split_at(count as usize * elem_size);
    *buf = rest;
    (count, bytes)
}

/// Writes one `count(4 LE) + bytes` proof section.
fn write_section(out: &mut Vec<u8>, count: u32, bytes: &[u8]) {
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(bytes);
}

/// The key the honest proof's membership `idx` queries.
fn honest_key(proof: &Proof<'_>, idx: usize) -> [u8; 32] {
    *proof.member(idx).unwrap().key
}

/// Mergeset context hash of the batch's chain block, as the verifier derives it.
fn context_hash() -> Hash {
    mergeset_context_hash(&MergesetContext {
        timestamp: PREV_TIMESTAMP,
        daa_score: DAA_SCORE,
        blue_score: BLUE_SCORE,
    })
}

/// Encodes one v1 tx journal: an input commitment over `resources` followed by a success output
/// commitment carrying `exits` and one entry per `outputs` element (`Some(hash)` is `Changed`).
fn tx_journal(
    tx_id: &Hash,
    merge_idx: u32,
    context_hash: &Hash,
    resources: &[(u32, [u8; 32], [u8; 32])],
    exits: &[(StandardSpk<'_>, u64)],
    outputs: &[Option<[u8; 32]>],
) -> Vec<u8> {
    let mut buf = Vec::new();

    // Input commitment: version, tx_id, merge_idx, then the v1 execution context.
    buf.extend_from_slice(&Transaction::V1.to_le_bytes());
    buf.extend_from_slice(tx_id.as_slice());
    buf.extend_from_slice(&merge_idx.to_le_bytes());
    buf.extend_from_slice(context_hash.as_slice());
    buf.extend_from_slice(&(resources.len() as u32).to_le_bytes());
    for (index, id, hash) in resources {
        buf.extend_from_slice(&index.to_le_bytes());
        buf.extend_from_slice(id);
        buf.extend_from_slice(hash);
    }

    // Output commitment: success, exits blob, deposit hash, then the resource entries.
    let mut exit_bytes = Vec::new();
    for (dest, amount) in exits {
        dest.encode(&mut exit_bytes);
        exit_bytes.extend_from_slice(&amount.to_le_bytes());
    }
    buf.push(OutputCommitment::SUCCESS);
    buf.extend_from_slice(&(exit_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(&exit_bytes);
    buf.extend_from_slice(&[0u8; 32]);
    for output in outputs {
        match output {
            Some(hash) => {
                buf.push(OutputResourceCommitment::CHANGED);
                buf.extend_from_slice(hash);
            }
            None => buf.push(OutputResourceCommitment::UNCHANGED),
        }
    }

    buf
}

/// Encodes batch-processor input bytes over `proof` and `tx_journals`.
fn batch_inputs(proof: &[u8], tx_journals: &[Vec<u8>]) -> Vec<u8> {
    let mut buf = Vec::new();

    // Bundle-constant pins.
    buf.extend_from_slice(&[0x01; 32]); // tx_image_id
    buf.extend_from_slice(&[0x02; 32]); // covenant_id
    buf.extend_from_slice(&[0u8; 32]); // deposit_spk_hash: this batch credits no deposit
    buf.extend_from_slice(&[0x03; 32]); // lane_key

    buf.extend_from_slice(&(proof.len() as u32).to_le_bytes());
    buf.extend_from_slice(proof);

    // Batch: per-block context followed by the length-prefixed tx journals.
    buf.extend_from_slice(&BLUE_SCORE.to_le_bytes());
    buf.extend_from_slice(&DAA_SCORE.to_le_bytes());
    buf.extend_from_slice(&PREV_TIMESTAMP.to_le_bytes());
    buf.extend_from_slice(&[0x04; 32]); // prev_seq_commit
    buf.extend_from_slice(&[0x05; 32]); // prev_lane_tip
    buf.extend_from_slice(&PREV_LANE_BLUE_SCORE.to_le_bytes());
    buf.push(0); // lane_expired

    let mut journals = Vec::new();
    for journal in tx_journals {
        journals.extend_from_slice(&(journal.len() as u32).to_le_bytes());
        journals.extend_from_slice(journal);
    }
    buf.extend_from_slice(&(journals.len() as u32).to_le_bytes());
    buf.extend_from_slice(&journals);

    buf
}

/// Renders the first eight bytes of a hash for failure messages.
fn hex(bytes: &[u8; 32]) -> String {
    bytes[..8].iter().map(|b| format!("{b:02x}")).collect()
}
