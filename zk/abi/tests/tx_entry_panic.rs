//! Reproducers for the guest-entry rejection contract: `process_transaction` rejects malformed
//! wire input through the journal instead of panicking.
//!
//! Every input fed here is a length-prefixed blob exactly like the one the executor writes to
//! the guest: in production those bytes are assembled host-side, but the payload, access
//! metadata, resource data, and tx id they embed all originate in an L1 transaction any user
//! can author. A decode failure must surface as `OutputCommitment::Error` + `Outputs::ERR`
//! (the unknown-version case proves that machinery works), never as a panic: a panic aborts
//! the executor call for every carrier in the batch, not just the offending one.
//!
//! A decode-rejected V1 carrier journals version 0 with no execution context (a V1 journal
//! entry without a context would not decode verifier-side) while keeping the header's tx id
//! and merge_idx, so the rejection stays attributed and the strictly-increasing merge_idx
//! ordering holds.

use kaspa_hashes::Hash;
use vprogs_core_codec::Writer;
use vprogs_core_hashing::Sha256;
use vprogs_l1_utils::tx_id_v1;
use vprogs_zk_abi::{
    Error, ErrorCode,
    transaction_processor::{JournalEntries, OutputCommitment, process_transaction},
};

/// Native stand-in for the risc0 host ABI: serves the input blob, collects stdout.
struct TestHost {
    input: Vec<u8>,
    stdout: Vec<u8>,
}

impl vprogs_zk_abi::Read for TestHost {
    fn read_blob(&mut self) -> Vec<u8> {
        self.input.clone()
    }
}

impl vprogs_core_codec::Writer for TestHost {
    fn write(&mut self, buf: &[u8]) {
        self.stdout.extend_from_slice(buf);
    }
}

/// One access-metadata entry: 32-byte resource id + 1-byte access type.
fn am(id: [u8; 32], write: bool) -> [u8; 33] {
    let mut out = [0u8; 33];
    out[..32].copy_from_slice(&id);
    out[32] = write as u8;
    out
}

/// Builds the host-input wire format around the given payload bytes and access metadata.
fn wire(
    version: u16,
    tx_id: [u8; 32],
    merge_idx: u32,
    access_metadata: &[[u8; 33]],
    ix_data: &[u8],
) -> Vec<u8> {
    // payload = access_metadata || ix_data
    let mut payload = Vec::new();
    payload.extend_from_slice(&(access_metadata.len() as u32).to_le_bytes());
    for entry in access_metadata {
        payload.extend_from_slice(&entry[..]);
    }
    payload.extend_from_slice(ix_data);

    // rest_preimage: minimal well-formed V1 bytes (version + counts + lock_time .. payload_len 0).
    let mut rest = Vec::new();
    rest.extend_from_slice(&1u16.to_le_bytes()); // tx version
    rest.extend_from_slice(&0u64.to_le_bytes()); // n_inputs
    rest.extend_from_slice(&0u64.to_le_bytes()); // n_outputs
    rest.extend_from_slice(&0u64.to_le_bytes()); // lock_time
    rest.extend_from_slice(&[0u8; 20]); // subnetwork_id
    rest.extend_from_slice(&0u64.to_le_bytes()); // gas
    rest.extend_from_slice(&0u64.to_le_bytes()); // payload len (excluded)

    let tx_id = if tx_id == [0xFF; 32] { tx_id_v1(&payload, &rest) } else { tx_id };

    let mut buf = Vec::new();
    buf.extend_from_slice(&version.to_le_bytes());
    buf.extend_from_slice(&tx_id[..]);
    buf.extend_from_slice(&merge_idx.to_le_bytes());
    // execution_input (V1)
    buf.extend_from_slice(&[0u8; 24]); // MergesetContext
    // tx blob
    let mut tx = Vec::new();
    tx.write_blob(&payload);
    tx.write_blob(&rest);
    buf.write_blob(&tx);
    // one resource per access-metadata entry, in order
    for (i, _) in access_metadata.iter().enumerate() {
        buf.extend_from_slice(&(i as u32).to_le_bytes());
        buf.write_blob(&[7u8; 8]); // resource data
    }
    buf
}

fn run(host_input: Vec<u8>) -> (TestHost, Vec<u8>) {
    let mut host = TestHost { input: host_input, stdout: Vec::new() };
    let mut journal = Vec::new();
    process_transaction::<Sha256>(
        &mut host,
        &mut journal,
        |_tx, _merge_idx, _context, _resources, _exits, _deposit| Ok(()),
    );
    (host, journal)
}

/// `run` variant whose handler writes the first resource, like the dummy counter guest writes
/// every resource regardless of declaration.
fn run_writing(host_input: Vec<u8>) -> (TestHost, Vec<u8>) {
    let mut host = TestHost { input: host_input, stdout: Vec::new() };
    let mut journal = Vec::new();
    process_transaction::<Sha256>(
        &mut host,
        &mut journal,
        |_tx, _merge_idx, _context, resources, _exits, _deposit| {
            resources[0].data_mut().copy_from_slice(&[9u8; 8]);
            Ok(())
        },
    );
    (host, journal)
}

/// Asserts the guest rejected: stdout carries the ERR discriminant, the journal decodes, and
/// the output commitment is the given error shape. Returns the decoded entries so callers can
/// assert on the input commitment.
fn rejected<'a>(host: &TestHost, journal: &'a [u8]) -> JournalEntries<'a> {
    assert_eq!(host.stdout.first(), Some(&1), "stdout discriminant must be ERR");
    let entries = JournalEntries::decode(journal).expect("journal decodes");
    assert!(matches!(entries.output_commitment, OutputCommitment::Error(_)));
    entries
}

/// Control case: a well-formed V1 input executes and commits Success. Proves the harness
/// builds valid wire bytes, so the rejection cases below fail on their specific defect.
#[test]
fn well_formed_v1_commits_success() {
    let (host, journal) = run(wire(1, [0xFF; 32], 0, &[am([1; 32], false)], &[]));
    assert_eq!(host.stdout.first(), Some(&0), "stdout discriminant must be OK");
    let entries = JournalEntries::decode(&journal).expect("journal decodes");
    assert!(matches!(entries.output_commitment, OutputCommitment::Success { .. }));
}

/// Control case: an unsupported version is rejected through the journal, no panic. This is
/// the exact machinery a decode failure must also reach.
#[test]
fn unknown_version_commits_error_without_panicking() {
    let (host, journal) = run(wire(2, [0xAB; 32], 0, &[], &[]));
    let entries = rejected(&host, &journal);
    assert!(entries.input_commitment.execution_context.is_none());
}

/// The observed tn10 incident: an access list that is not strictly ascending (a web-built
/// transfer to self) is rejected through the journal, with the rejection attributed to the
/// header's real tx id and merge_idx.
#[test]
fn non_ascending_access_list_commits_error() {
    let input =
        wire(1, [0x9A; 32], 3, &[am([5; 32], true), am([5; 32], true), am([9; 32], false)], &[]);
    let (host, journal) = run(input);
    let entries = rejected(&host, &journal);
    assert!(matches!(entries.output_commitment, OutputCommitment::Error(Error::Decode(_))));
    assert_eq!(entries.input_commitment.version, 0);
    assert_eq!(entries.input_commitment.tx_id, &Hash::from_bytes([0x9A; 32]));
    assert_eq!(entries.input_commitment.merge_idx, 3);
    assert!(entries.input_commitment.execution_context.is_none());
}

/// A truncated wire buffer (resource section cut short) is rejected the same way, keeping the
/// header's tx id and merge_idx.
#[test]
fn truncated_input_commits_error() {
    let mut input = wire(1, [0x9B; 32], 7, &[am([1; 32], false), am([2; 32], false)], &[]);
    input.truncate(input.len() - 6);
    let (host, journal) = run(input);
    let entries = rejected(&host, &journal);
    assert!(matches!(entries.output_commitment, OutputCommitment::Error(Error::Decode(_))));
    assert_eq!(entries.input_commitment.version, 0);
    assert_eq!(entries.input_commitment.tx_id, &Hash::from_bytes([0x9B; 32]));
    assert_eq!(entries.input_commitment.merge_idx, 7);
    assert!(entries.input_commitment.execution_context.is_none());
}

/// A host-supplied tx id that disagrees with the derived one rejects after the input
/// commitment is journaled, with the execution context still present.
#[test]
fn tx_id_mismatch_commits_error() {
    let (host, journal) = run(wire(1, [0xEE; 32], 0, &[am([1; 32], false)], &[]));
    let entries = rejected(&host, &journal);
    assert!(matches!(entries.output_commitment, OutputCommitment::Error(Error::Decode(_))));
    assert_eq!(entries.input_commitment.version, 1);
    assert_eq!(entries.input_commitment.tx_id, &Hash::from_bytes([0xEE; 32]));
    assert!(entries.input_commitment.execution_context.is_some());
}

/// A handler that acquires mutable access to a `Read`-declared resource rejects the whole
/// transaction (#107). Declarations come from sender-supplied L1 payload, so the mismatch is
/// user-reachable, not a program bug: journaling the write would settle a root the host store
/// (which honors the declaration) never records, permanently wedging every later bundle. The
/// rejection stays attributed: the tx executed, so the execution context remains in the input
/// commitment.
#[test]
fn writing_a_read_declared_resource_rejects() {
    let (host, journal) = run_writing(wire(1, [0xFF; 32], 0, &[am([1; 32], false)], &[]));
    let entries = rejected(&host, &journal);
    assert!(matches!(
        entries.output_commitment,
        OutputCommitment::Error(Error::Guest(code)) if code == ErrorCode::ReadDeclaredWrite as u32
    ));
    assert_eq!(entries.input_commitment.version, 1);
    assert!(entries.input_commitment.execution_context.is_some());
}

/// Control for the rejection above: the identical write under a `Write` declaration commits
/// Success, so the rejection is attributable to the declaration, not the write itself.
#[test]
fn writing_a_write_declared_resource_commits_success() {
    let (host, journal) = run_writing(wire(1, [0xFF; 32], 0, &[am([1; 32], true)], &[]));
    assert_eq!(host.stdout.first(), Some(&0), "stdout discriminant must be OK");
    let entries = JournalEntries::decode(&journal).expect("journal decodes");
    assert!(matches!(entries.output_commitment, OutputCommitment::Success { .. }));
}
