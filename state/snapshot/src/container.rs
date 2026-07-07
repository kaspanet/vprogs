use std::io::{Read, Write};

use sha2::Digest;
use vprogs_core_hashing::{Hasher, Sha256};
use vprogs_core_types::ResourceId;

/// Magic + embedded format tag. Bump the trailing digits on any breaking layout change.
pub const MAGIC: [u8; 8] = *b"VPSNAP01";
/// Container framing version. Readers reject unknown versions.
pub const FORMAT_VERSION: u16 = 1;
/// Defensive cap on the total bytes `read_container` will pull from its reader before it has
/// validated anything. This crate parses externally-shared, untrusted files; the cap bounds
/// memory use for a hostile or truncated-but-enormous input without constraining any real
/// snapshot we expect to produce.
pub const MAX_SNAPSHOT_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Minimum on-wire size of one record: a 32-byte `resource_id` plus a `u32` `value_len` (the
/// value itself may be empty). Used to bound a file-declared `record_count` against the bytes
/// actually remaining in the body, so we never allocate a `Vec` sized by an untrusted count.
const MIN_RECORD_LEN: u64 = 32 + 4;

/// One resource's latest state: an opaque id and opaque value bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub resource_id: ResourceId,
    pub value: Vec<u8>,
}

/// Errors from reading or writing a snapshot container.
#[derive(Debug)]
pub enum SnapshotError {
    /// I/O failure while reading or writing the container.
    Io(std::io::Error),
    /// Input's leading bytes do not match [`MAGIC`]; not a vprogs snapshot.
    BadMagic,
    /// Body declares a format version this reader does not support.
    UnsupportedVersion(u16),
    /// Trailing digest does not match the body (corrupted in transit or at rest).
    DigestMismatch,
    /// Stream ended before a fixed-size field could be fully read.
    Truncated,
    /// A length-prefixed field (header, record count, or value) declares more bytes than
    /// remain in the body. Distinct from `Truncated`, which means the stream ended before a
    /// fixed-size field could even be read.
    Malformed(&'static str),
    /// `write_container` was asked to frame a header or value whose length does not fit in
    /// the on-wire `u32` field.
    FieldTooLarge,
}

impl From<std::io::Error> for SnapshotError {
    fn from(e: std::io::Error) -> Self {
        SnapshotError::Io(e)
    }
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapshotError::Io(e) => write!(f, "snapshot io error: {e}"),
            SnapshotError::BadMagic => write!(f, "not a vprogs snapshot (bad magic)"),
            SnapshotError::UnsupportedVersion(v) => {
                write!(f, "unsupported snapshot format version {v}")
            }
            SnapshotError::DigestMismatch => {
                write!(f, "snapshot digest mismatch (corrupted in transit or at rest)")
            }
            SnapshotError::Truncated => write!(f, "snapshot truncated"),
            SnapshotError::Malformed(what) => write!(f, "snapshot malformed: {what}"),
            SnapshotError::FieldTooLarge => {
                write!(f, "snapshot field exceeds the on-wire u32 length limit")
            }
        }
    }
}

impl std::error::Error for SnapshotError {}

/// A writer that forwards bytes to an inner sink while folding them into a SHA-256 digest.
struct HashingWriter<'a, W: Write> {
    inner: &'a mut W,
    hasher: sha2::Sha256,
}

impl<W: Write> Write for HashingWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.hasher.update(buf);
        self.inner.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Frame `header` and `records` into the container format documented on [`MAGIC`], sealed with
/// a trailing SHA-256 digest of the body. The digest guards against accidental corruption in
/// transit or at rest only; it is recomputed by any producer and so authenticates nothing.
/// `record_count` must equal the number of items `records` yields; this is checked with a
/// `debug_assert` since callers control both from the same iteration.
///
/// Returns [`SnapshotError::FieldTooLarge`] if `header` or any record value is longer than
/// `u32::MAX`, rather than silently truncating the on-wire length prefix.
pub fn write_container<W: Write>(
    w: &mut W,
    header: &[u8],
    record_count: u64,
    records: impl IntoIterator<Item = Record>,
) -> Result<(), SnapshotError> {
    let header_len: u32 = header.len().try_into().map_err(|_| SnapshotError::FieldTooLarge)?;

    let mut hw = HashingWriter { inner: w, hasher: sha2::Sha256::new() };

    hw.write_all(&MAGIC)?;
    hw.write_all(&FORMAT_VERSION.to_le_bytes())?;
    hw.write_all(&header_len.to_le_bytes())?;
    hw.write_all(header)?;
    hw.write_all(&record_count.to_le_bytes())?;

    let mut written = 0u64;
    for rec in records {
        let value_len: u32 =
            rec.value.len().try_into().map_err(|_| SnapshotError::FieldTooLarge)?;
        hw.write_all(rec.resource_id.as_slice())?;
        hw.write_all(&value_len.to_le_bytes())?;
        hw.write_all(&rec.value)?;
        written += 1;
    }
    debug_assert_eq!(written, record_count, "record iterator disagreed with record_count");

    let digest = hw.hasher.finalize();
    hw.inner.write_all(&digest)?;
    Ok(())
}

/// Parse and verify a container written by [`write_container`]. Rejects unknown magic,
/// unsupported format versions, a truncated stream, or a digest that does not match the body.
///
/// `r` is untrusted, externally-shared input: this function never trusts a file-declared
/// length enough to allocate by it directly. Every length-prefixed field (header, record
/// count, value) is bounds-checked against the bytes actually remaining in the body before
/// any allocation is sized by it, and the input itself is capped at [`MAX_SNAPSHOT_BYTES`].
pub fn read_container<R: Read>(r: &mut R) -> Result<(Vec<u8>, Vec<Record>), SnapshotError> {
    // Read the whole stream so we can both parse and verify the trailing digest, but never
    // more than the defensive cap: a hostile or corrupt-but-endless stream must not be
    // buffered in full before we get a chance to reject it.
    let mut all = Vec::new();
    let mut capped = r.take(MAX_SNAPSHOT_BYTES);
    capped.read_to_end(&mut all)?;
    if all.len() as u64 == MAX_SNAPSHOT_BYTES {
        // Ambiguous whether the stream ended exactly at the cap or kept going; either way an
        // honest snapshot is never this large, so treat it as oversized input.
        let mut probe = [0u8; 1];
        if capped.into_inner().read(&mut probe)? > 0 {
            return Err(SnapshotError::Malformed("input exceeds MAX_SNAPSHOT_BYTES"));
        }
    }

    // Reject non-snapshot input on magic alone, before requiring a full-length body or a
    // matching digest: a foreign file may be too short, or long but with an unrelated
    // trailing 32 bytes that never hashes to its own body.
    if all.len() < MAGIC.len() {
        return Err(SnapshotError::Truncated);
    }
    if all[..MAGIC.len()] != MAGIC {
        return Err(SnapshotError::BadMagic);
    }

    if all.len() < MAGIC.len() + 2 + 32 {
        return Err(SnapshotError::Truncated);
    }
    let (body, digest) = all.split_at(all.len() - 32);
    let digest: [u8; 32] = digest.try_into().map_err(|_| SnapshotError::Truncated)?;
    if Sha256::hash(body) != digest {
        return Err(SnapshotError::DigestMismatch);
    }

    let mut cur = std::io::Cursor::new(body);
    cur.set_position(MAGIC.len() as u64);
    let version = read_u16(&mut cur)?;
    if version != FORMAT_VERSION {
        return Err(SnapshotError::UnsupportedVersion(version));
    }

    let header_len = read_u32(&mut cur)? as u64;
    if header_len > remaining(&cur) {
        return Err(SnapshotError::Malformed("header_len exceeds remaining body"));
    }
    let mut header = vec![0u8; header_len as usize];
    read_exact(&mut cur, &mut header)?;

    let record_count = read_u64(&mut cur)?;
    // Each record occupies at least MIN_RECORD_LEN bytes on the wire (id + value_len, value
    // itself may be empty), so a record_count that could not possibly fit in what remains is
    // rejected before any allocation is sized by it.
    if record_count > remaining(&cur) / MIN_RECORD_LEN {
        return Err(SnapshotError::Malformed("record_count exceeds remaining body"));
    }
    let mut records = Vec::new();
    for _ in 0..record_count {
        let mut id = [0u8; 32];
        read_exact(&mut cur, &mut id)?;
        let value_len = read_u32(&mut cur)? as u64;
        if value_len > remaining(&cur) {
            return Err(SnapshotError::Malformed("record value_len exceeds remaining body"));
        }
        let mut value = vec![0u8; value_len as usize];
        read_exact(&mut cur, &mut value)?;
        records.push(Record { resource_id: ResourceId::from(id), value });
    }
    Ok((header, records))
}

/// Bytes remaining to be read from `cur`, given its current position and total length.
fn remaining(cur: &std::io::Cursor<&[u8]>) -> u64 {
    (cur.get_ref().len() as u64).saturating_sub(cur.position())
}

/// Fills `buf` completely, mapping any short read to [`SnapshotError::Truncated`].
fn read_exact<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<(), SnapshotError> {
    r.read_exact(buf).map_err(|_| SnapshotError::Truncated)
}
/// Reads a little-endian `u16`.
fn read_u16<R: Read>(r: &mut R) -> Result<u16, SnapshotError> {
    let mut b = [0u8; 2];
    read_exact(r, &mut b)?;
    Ok(u16::from_le_bytes(b))
}
/// Reads a little-endian `u32`.
fn read_u32<R: Read>(r: &mut R) -> Result<u32, SnapshotError> {
    let mut b = [0u8; 4];
    read_exact(r, &mut b)?;
    Ok(u32::from_le_bytes(b))
}
/// Reads a little-endian `u64`.
fn read_u64<R: Read>(r: &mut R) -> Result<u64, SnapshotError> {
    let mut b = [0u8; 8];
    read_exact(r, &mut b)?;
    Ok(u64::from_le_bytes(b))
}

#[cfg(test)]
mod tests {
    use vprogs_core_types::ResourceId;

    use super::*;

    fn rec(b: u8, v: &[u8]) -> Record {
        Record { resource_id: ResourceId::from([b; 32]), value: v.to_vec() }
    }

    #[test]
    fn container_roundtrips_header_and_records() {
        let header = b"opaque-header-bytes".to_vec();
        let records = vec![rec(1, b"alpha"), rec(2, b""), rec(3, b"gamma-value")];

        let mut buf = Vec::new();
        write_container(&mut buf, &header, records.len() as u64, records.clone()).unwrap();

        let (got_header, got_records) = read_container(&mut buf.as_slice()).unwrap();
        assert_eq!(got_header, header);
        assert_eq!(got_records, records);
    }

    #[test]
    fn corrupted_digest_is_rejected() {
        let mut buf = Vec::new();
        write_container(&mut buf, b"h", 1, vec![rec(9, b"x")]).unwrap();
        let last = buf.len() - 1;
        buf[last] ^= 0xff; // flip a digest byte
        assert!(matches!(read_container(&mut buf.as_slice()), Err(SnapshotError::DigestMismatch)));
    }

    #[test]
    fn bad_magic_is_rejected() {
        let buf = vec![0u8; 8];
        assert!(matches!(read_container(&mut buf.as_slice()), Err(SnapshotError::BadMagic)));
    }

    /// Appends a correct SHA-256 digest to a hand-crafted body, producing a byte stream that
    /// passes the digest check regardless of whether the framed fields inside `body` make
    /// sense. Lets the adversarial tests below control every framing field directly instead of
    /// going through `write_container`.
    fn seal(body: Vec<u8>) -> Vec<u8> {
        let digest = Sha256::hash(&body);
        let mut sealed = body;
        sealed.extend_from_slice(&digest);
        sealed
    }

    /// Regression test for the review finding: a forged file can declare an absurd
    /// `record_count` (here `u64::MAX`) while still being tiny and digest-valid. Reading it
    /// must return an error, never panic (capacity overflow) or abort (alloc failure).
    #[test]
    fn oversized_record_count_is_rejected_without_panic() {
        let mut body = Vec::new();
        body.extend_from_slice(&MAGIC);
        body.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // header_len = 0
        body.extend_from_slice(&u64::MAX.to_le_bytes()); // record_count = u64::MAX
        let buf = seal(body);

        let result = read_container(&mut buf.as_slice());
        assert!(
            matches!(result, Err(SnapshotError::Malformed(_))),
            "expected Malformed, got {result:?}"
        );
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&MAGIC);
        body.extend_from_slice(&999u16.to_le_bytes()); // unknown version
        body.extend_from_slice(&0u32.to_le_bytes()); // header_len = 0
        body.extend_from_slice(&0u64.to_le_bytes()); // record_count = 0
        let buf = seal(body);

        assert!(matches!(
            read_container(&mut buf.as_slice()),
            Err(SnapshotError::UnsupportedVersion(999))
        ));
    }

    /// A body whose declared `record_count` fits the aggregate bound but whose second record is
    /// cut off before its `value_len` field: the bound check on the aggregate byte count alone
    /// cannot catch this, since it only guarantees room for each record's *minimum* footprint,
    /// not that an earlier record's larger value hasn't already eaten into it. The `read_exact`
    /// for the second record's `value_len` then runs out of bytes.
    #[test]
    fn truncated_mid_record_is_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&MAGIC);
        body.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // header_len = 0
        body.extend_from_slice(&2u64.to_le_bytes()); // record_count = 2

        // Record 1: full record, 4-byte value (32 + 4 + 4 = 40 bytes).
        body.extend_from_slice(&[1u8; 32]);
        body.extend_from_slice(&4u32.to_le_bytes());
        body.extend_from_slice(&[0xAA; 4]);

        // Record 2: only the 32-byte id is present; value_len and value are missing. The total
        // remaining after record_count (40 + 32 = 72) exactly meets the 2 * 36-byte aggregate
        // floor, so the upfront bound check passes and parsing must fail later, mid-record.
        body.extend_from_slice(&[2u8; 32]);

        let buf = seal(body);
        let result = read_container(&mut buf.as_slice());
        assert!(
            matches!(result, Err(SnapshotError::Truncated)),
            "expected Truncated, got {result:?}"
        );
    }

    #[test]
    fn long_foreign_file_is_bad_magic_not_digest_mismatch() {
        let buf = vec![0xABu8; 200];
        assert!(matches!(read_container(&mut buf.as_slice()), Err(SnapshotError::BadMagic)));
    }

    #[test]
    fn oversized_header_len_is_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&MAGIC);
        body.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        body.extend_from_slice(&u32::MAX.to_le_bytes()); // header_len declares 4 GiB
        let buf = seal(body);

        let result = read_container(&mut buf.as_slice());
        assert!(
            matches!(result, Err(SnapshotError::Malformed(_))),
            "expected Malformed, got {result:?}"
        );
    }

    #[test]
    fn oversized_value_len_is_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&MAGIC);
        body.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // header_len = 0
        body.extend_from_slice(&1u64.to_le_bytes()); // record_count = 1
        body.extend_from_slice(&[3u8; 32]); // resource_id
        body.extend_from_slice(&u32::MAX.to_le_bytes()); // value_len declares 4 GiB
        let buf = seal(body);

        let result = read_container(&mut buf.as_slice());
        assert!(
            matches!(result, Err(SnapshotError::Malformed(_))),
            "expected Malformed, got {result:?}"
        );
    }
}
