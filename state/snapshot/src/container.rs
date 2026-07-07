//! Streaming snapshot codec: an opaque header plus a stream of `(resource_id, value)` records,
//! framed with a trailing digest over everything before it.
//!
//! Wire layout (little-endian integers):
//! `MAGIC[8] | version:u16 | header_len:u32 | header[header_len] | record_count:u64 |
//! { id[32] | value_len:u32 | value[value_len] }*record_count | digest[32]`
//!
//! [`write_snapshot`] and [`SnapshotReader`] never hold the whole file in memory: the writer
//! streams `records` straight to `w` as it iterates them, and the reader yields one record at a
//! time from [`SnapshotReader::next`], with at most one record's value resident at once. Both
//! sides fold the bytes they produce/consume into `H::incremental()` so the trailing digest never
//! requires a second pass over the body.

use std::io::{Read, Write};

use vprogs_core_hashing::{Hasher, IncrementalHasher};
use vprogs_core_types::ResourceId;

/// Magic + embedded format tag. Bump the trailing digits on any breaking layout change.
pub const MAGIC: [u8; 8] = *b"VPSNAP01";
/// Framing version. Readers reject unknown versions.
pub const FORMAT_VERSION: u16 = 1;
/// Cap on the opaque header region. The header carries caller metadata (small and structured),
/// never resource values, so 1 MiB is generous while still bounding the allocation
/// [`SnapshotReader::open`] performs before it has validated anything else about the file.
pub const MAX_HEADER_LEN: u32 = 1 << 20;
/// Per-record value cap, enforced by [`SnapshotReader::next`] before it allocates anything sized
/// by the untrusted on-wire `value_len`. 256 MiB is generous for a single resource blob under this
/// account/state model; a program that legitimately needs a larger single value should bump this
/// constant rather than work around it.
pub const MAX_VALUE_LEN: u32 = 256 * 1024 * 1024;

/// One resource's latest state: an opaque id and opaque value bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub resource_id: ResourceId,
    pub value: Vec<u8>,
}

/// Errors from reading or writing a snapshot.
#[derive(Debug)]
pub enum SnapshotError {
    /// I/O failure while reading or writing the snapshot.
    Io(std::io::Error),
    /// Input's leading bytes do not match [`MAGIC`]; not a vprogs snapshot.
    BadMagic,
    /// Body declares a format version this reader does not support.
    UnsupportedVersion(u16),
    /// Trailing digest does not match the body (corrupted in transit or at rest).
    DigestMismatch,
    /// Stream ended before a fixed-size or declared-length field could be fully read.
    Truncated,
    /// A length-prefixed field declares a value this reader refuses on its face (e.g.
    /// `header_len > MAX_HEADER_LEN` or `value_len > MAX_VALUE_LEN`), independent of how many
    /// bytes the stream actually holds; also returned by [`SnapshotReader::finish`] for a
    /// structurally invalid call (early finish, or trailing bytes after the digest).
    Malformed(&'static str),
    /// [`write_snapshot`] was asked to frame a header or value whose length does not fit in the
    /// on-wire `u32` field.
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

/// Forwards writes to `inner`, folding exactly the bytes `inner` accepts into `hasher`. Folding
/// only the accepted bytes (not the whole input slice up front) keeps the digest correct even if
/// `inner` ever performs a short write, since `write_all`'s retry loop then re-offers only the
/// unwritten remainder.
struct HashingWriter<'a, W: Write, Inc: IncrementalHasher> {
    /// Sink every write is forwarded to.
    inner: &'a mut W,
    /// Running digest over the bytes actually forwarded so far.
    hasher: Inc,
}

impl<W: Write, Inc: IncrementalHasher> Write for HashingWriter<'_, W, Inc> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Streams `header` and `records` to `w` in the layout documented on the module, sealed with a
/// trailing digest folded incrementally with `H`. The digest guards against accidental corruption
/// in transit or at rest only; it is recomputed by any producer and so authenticates nothing.
///
/// `header` is opaque; interpreting it is the caller's responsibility (e.g. the runner's encoded
/// typed header). `records` MUST be sorted by non-decreasing `resource_id` and yield exactly
/// `record_count` items; both are checked with a `debug_assert` (sortedness per consecutive pair,
/// count once the iterator is drained) since callers control both from the same enumeration and
/// this is not re-validated in release builds.
///
/// Returns [`SnapshotError::FieldTooLarge`] if `header` or any record value is longer than
/// `u32::MAX`, rather than silently truncating the on-wire length prefix.
pub fn write_snapshot<W: Write, H: Hasher>(
    w: &mut W,
    header: &[u8],
    record_count: u64,
    records: impl IntoIterator<Item = Record>,
) -> Result<(), SnapshotError> {
    let header_len: u32 = header.len().try_into().map_err(|_| SnapshotError::FieldTooLarge)?;

    let mut hw = HashingWriter::<_, H::Incremental> { inner: w, hasher: H::incremental() };

    hw.write_all(&MAGIC)?;
    hw.write_all(&FORMAT_VERSION.to_le_bytes())?;
    hw.write_all(&header_len.to_le_bytes())?;
    hw.write_all(header)?;
    hw.write_all(&record_count.to_le_bytes())?;

    let mut written = 0u64;
    #[cfg(debug_assertions)]
    let mut prev_id: Option<ResourceId> = None;
    for rec in records {
        let value_len: u32 =
            rec.value.len().try_into().map_err(|_| SnapshotError::FieldTooLarge)?;
        // Sortedness check against the previous record; compiled out entirely in release builds.
        #[cfg(debug_assertions)]
        {
            if let Some(prev) = prev_id {
                debug_assert!(
                    prev <= rec.resource_id,
                    "records not sorted by resource_id: {prev:?} appeared before {:?}",
                    rec.resource_id
                );
            }
            prev_id = Some(rec.resource_id);
        }
        hw.write_all(rec.resource_id.as_slice())?;
        hw.write_all(&value_len.to_le_bytes())?;
        hw.write_all(&rec.value)?;
        written += 1;
    }
    debug_assert_eq!(written, record_count, "record iterator disagreed with record_count");

    let HashingWriter { inner, hasher } = hw;
    inner.write_all(&hasher.finalize())?;
    Ok(())
}

/// Streaming reader over a snapshot produced by [`write_snapshot`]. The whole file is never
/// buffered: [`open`](Self::open) reads and validates only the fixed prefix, [`next`](Self::next)
/// reads one record at a time via a bounded `read_exact`, and [`finish`](Self::finish) checks the
/// trailing digest once all records have been consumed.
pub struct SnapshotReader<R: Read, H: Hasher> {
    /// Underlying byte stream, positioned just past the last byte consumed.
    reader: R,
    /// Running digest over every byte read so far, magic through the last record.
    hasher: H::Incremental,
    /// `record_count` declared by the header, fixed at [`open`](Self::open) time.
    record_count: u64,
    /// Records not yet yielded by [`next`](Self::next).
    remaining: u64,
}

impl<R: Read, H: Hasher> SnapshotReader<R, H> {
    /// Reads and validates the fixed prefix (`magic`, `version`, `header_len`, `header`,
    /// `record_count`), folding every byte read into the running digest, and returns the opaque
    /// header. Rejects unknown magic or an unsupported version before ever looking at
    /// `header_len`, and rejects a `header_len` over [`MAX_HEADER_LEN`] before allocating the
    /// header buffer.
    pub fn open(mut r: R) -> Result<(Vec<u8>, Self), SnapshotError> {
        let mut hasher = H::incremental();

        let mut magic = [0u8; 8];
        read_exact_fold(&mut r, &mut magic, &mut hasher)?;
        if magic != MAGIC {
            return Err(SnapshotError::BadMagic);
        }

        let version = read_u16_fold(&mut r, &mut hasher)?;
        if version != FORMAT_VERSION {
            return Err(SnapshotError::UnsupportedVersion(version));
        }

        let header_len = read_u32_fold(&mut r, &mut hasher)?;
        if header_len > MAX_HEADER_LEN {
            return Err(SnapshotError::Malformed("header_len exceeds MAX_HEADER_LEN"));
        }
        let mut header = vec![0u8; header_len as usize];
        read_exact_fold(&mut r, &mut header, &mut hasher)?;

        let record_count = read_u64_fold(&mut r, &mut hasher)?;

        Ok((header, SnapshotReader { reader: r, hasher, record_count, remaining: record_count }))
    }

    /// The `record_count` declared by the header, fixed at [`open`](Self::open) time.
    pub fn record_count(&self) -> u64 {
        self.record_count
    }

    /// Reads the next record, or `Ok(None)` once all `record_count` records have been consumed.
    /// Reads exactly one `id` (32 bytes) then one declared-length `value`. Rejects a `value_len`
    /// over [`MAX_VALUE_LEN`] with [`SnapshotError::Malformed`] before allocating anything sized
    /// by it, and even within that cap never pre-allocates the declared length: the value is read
    /// through a bounded adapter that grows only with bytes actually observed, so a `value_len`
    /// the stream cannot back yields [`SnapshotError::Truncated`] instead of the buffer being
    /// pre-sized to a length the file never delivers.
    ///
    /// Named `next` rather than implemented as `Iterator` on purpose: the fallible,
    /// record-at-a-time shape is the point, and an `Iterator<Item = Result<Record, ..>>` adapter
    /// would let callers reach for `.collect()`, which is exactly the whole-file buffering this
    /// type exists to avoid.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Record>, SnapshotError> {
        if self.remaining == 0 {
            return Ok(None);
        }

        let mut id = [0u8; 32];
        read_exact_fold(&mut self.reader, &mut id, &mut self.hasher)?;

        let value_len = read_u32_fold(&mut self.reader, &mut self.hasher)?;
        if value_len > MAX_VALUE_LEN {
            return Err(SnapshotError::Malformed("record value_len exceeds MAX_VALUE_LEN"));
        }
        // Bounded by the `take` limit, not by `value_len` up front: the `Vec` this grows into is
        // only ever as large as the bytes actually read, so a hostile `value_len` within the cap
        // still can't force an allocation the stream doesn't back.
        let mut value = Vec::new();
        let n = self.reader.by_ref().take(value_len as u64).read_to_end(&mut value)?;
        if n as u64 != value_len as u64 {
            return Err(SnapshotError::Truncated);
        }
        self.hasher.update(&value);

        self.remaining -= 1;
        Ok(Some(Record { resource_id: ResourceId::from(id), value }))
    }

    /// Verifies the trailing digest against everything read so far. Callers must drive
    /// [`next`](Self::next) to `Ok(None)` first; calling `finish` early returns
    /// [`SnapshotError::Malformed`] instead of silently verifying a partial read as if it were
    /// the whole snapshot. Also rejects (`Malformed`) any bytes left in the reader once the
    /// digest has been consumed, so a file with a correct digest but junk appended after it does
    /// not "verify".
    pub fn finish(self) -> Result<(), SnapshotError> {
        if self.remaining != 0 {
            return Err(SnapshotError::Malformed("finish called before all records were read"));
        }
        let mut reader = self.reader;
        let mut digest = [0u8; 32];
        reader.read_exact(&mut digest).map_err(|_| SnapshotError::Truncated)?;
        if self.hasher.finalize() != digest {
            return Err(SnapshotError::DigestMismatch);
        }

        let mut probe = [0u8; 1];
        if reader.read(&mut probe)? != 0 {
            return Err(SnapshotError::Malformed("trailing bytes after digest"));
        }
        Ok(())
    }
}

/// Fills `buf` completely and folds the bytes read into `hasher`, mapping any short read to
/// [`SnapshotError::Truncated`].
fn read_exact_fold<R: Read, Inc: IncrementalHasher>(
    r: &mut R,
    buf: &mut [u8],
    hasher: &mut Inc,
) -> Result<(), SnapshotError> {
    r.read_exact(buf).map_err(|_| SnapshotError::Truncated)?;
    hasher.update(buf);
    Ok(())
}

/// Reads a little-endian `u16`, folding it into `hasher`.
fn read_u16_fold<R: Read, Inc: IncrementalHasher>(
    r: &mut R,
    hasher: &mut Inc,
) -> Result<u16, SnapshotError> {
    let mut b = [0u8; 2];
    read_exact_fold(r, &mut b, hasher)?;
    Ok(u16::from_le_bytes(b))
}

/// Reads a little-endian `u32`, folding it into `hasher`.
fn read_u32_fold<R: Read, Inc: IncrementalHasher>(
    r: &mut R,
    hasher: &mut Inc,
) -> Result<u32, SnapshotError> {
    let mut b = [0u8; 4];
    read_exact_fold(r, &mut b, hasher)?;
    Ok(u32::from_le_bytes(b))
}

/// Reads a little-endian `u64`, folding it into `hasher`.
fn read_u64_fold<R: Read, Inc: IncrementalHasher>(
    r: &mut R,
    hasher: &mut Inc,
) -> Result<u64, SnapshotError> {
    let mut b = [0u8; 8];
    read_exact_fold(r, &mut b, hasher)?;
    Ok(u64::from_le_bytes(b))
}

#[cfg(test)]
mod tests {
    use vprogs_core_hashing::Sha256;
    use vprogs_core_types::ResourceId;

    use super::*;

    fn rec(b: u8, v: &[u8]) -> Record {
        Record { resource_id: ResourceId::from([b; 32]), value: v.to_vec() }
    }

    #[test]
    fn streaming_round_trip_via_next() {
        let header = b"opaque-header-bytes".to_vec();
        let records = vec![rec(1, b"alpha"), rec(2, b""), rec(3, b"gamma-value")];

        let mut buf = Vec::new();
        write_snapshot::<_, Sha256>(&mut buf, &header, records.len() as u64, records.clone())
            .unwrap();

        let (got_header, mut reader) = SnapshotReader::<_, Sha256>::open(buf.as_slice()).unwrap();
        assert_eq!(got_header, header);
        assert_eq!(reader.record_count(), records.len() as u64);

        let mut got = Vec::new();
        while let Some(r) = reader.next().unwrap() {
            got.push(r);
        }
        reader.finish().unwrap();
        assert_eq!(got, records);
    }

    /// `records` must be sorted by non-decreasing `resource_id`; feeding them out of order trips
    /// the writer's `debug_assert` in debug builds rather than silently emitting an unsorted
    /// (and hence unreadable-as-canonical) file. This test only runs meaningfully in debug builds
    /// (`debug_assertions`), matching where the check is compiled in.
    #[test]
    #[should_panic(expected = "records not sorted by resource_id")]
    #[cfg_attr(not(debug_assertions), ignore = "debug_assert is compiled out in release builds")]
    fn unsorted_records_trip_debug_assert() {
        let records = vec![rec(2, b"beta"), rec(1, b"alpha")];
        let mut buf = Vec::new();
        let _ = write_snapshot::<_, Sha256>(&mut buf, b"h", records.len() as u64, records);
    }

    /// A file with a correct digest but extra bytes appended after it must not silently
    /// "verify": `finish` should notice the underlying reader isn't at EOF once the digest has
    /// been consumed and reject the trailing junk instead.
    #[test]
    fn trailing_bytes_after_digest_are_rejected() {
        let mut buf = Vec::new();
        write_snapshot::<_, Sha256>(&mut buf, b"h", 1, vec![rec(9, b"x")]).unwrap();
        buf.extend_from_slice(b"junk-appended-after-digest");

        let (_hdr, mut reader) = SnapshotReader::<_, Sha256>::open(buf.as_slice()).unwrap();
        while reader.next().unwrap().is_some() {}
        assert!(matches!(reader.finish(), Err(SnapshotError::Malformed(_))));
    }

    #[test]
    fn corrupted_digest_is_rejected() {
        let mut buf = Vec::new();
        write_snapshot::<_, Sha256>(&mut buf, b"h", 1, vec![rec(9, b"x")]).unwrap();
        let last = buf.len() - 1;
        buf[last] ^= 0xff; // flip a digest byte

        let (_hdr, mut reader) = SnapshotReader::<_, Sha256>::open(buf.as_slice()).unwrap();
        while reader.next().unwrap().is_some() {}
        assert!(matches!(reader.finish(), Err(SnapshotError::DigestMismatch)));
    }

    #[test]
    fn bad_magic_is_rejected() {
        let buf = vec![0u8; 8];
        assert!(matches!(
            SnapshotReader::<_, Sha256>::open(buf.as_slice()),
            Err(SnapshotError::BadMagic)
        ));
    }

    #[test]
    fn long_foreign_file_is_bad_magic_not_digest_mismatch() {
        let buf = vec![0xABu8; 200];
        assert!(matches!(
            SnapshotReader::<_, Sha256>::open(buf.as_slice()),
            Err(SnapshotError::BadMagic)
        ));
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&MAGIC);
        body.extend_from_slice(&999u16.to_le_bytes()); // unknown version

        let result = SnapshotReader::<_, Sha256>::open(body.as_slice());
        assert!(matches!(result, Err(SnapshotError::UnsupportedVersion(999))));
    }

    #[test]
    fn oversized_header_len_is_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&MAGIC);
        body.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        body.extend_from_slice(&(MAX_HEADER_LEN + 1).to_le_bytes());

        // Discard the `Ok` payload before formatting: `SnapshotReader` need not be `Debug`.
        let result = SnapshotReader::<_, Sha256>::open(body.as_slice()).map(|_| ());
        assert!(
            matches!(result, Err(SnapshotError::Malformed(_))),
            "expected Malformed, got {result:?}"
        );
    }

    /// Regression test for the review finding: a forged file can declare an absurd
    /// `record_count` (here `u64::MAX`) while still being tiny. The reader never preallocates
    /// anything sized by `record_count`; the first `next()` call simply runs out of stream while
    /// reading the first record's `id` and must return an error, never panic (capacity overflow)
    /// or abort (alloc failure).
    #[test]
    fn oversized_record_count_is_rejected_without_panic() {
        let mut body = Vec::new();
        body.extend_from_slice(&MAGIC);
        body.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // header_len = 0
        body.extend_from_slice(&u64::MAX.to_le_bytes()); // record_count = u64::MAX

        let (_hdr, mut reader) = SnapshotReader::<_, Sha256>::open(body.as_slice()).unwrap();
        let result = reader.next();
        assert!(
            matches!(result, Err(SnapshotError::Truncated)),
            "expected Truncated, got {result:?}"
        );
    }

    /// Regression test for the review finding: a forged record can declare a `value_len` far
    /// beyond anything the tiny file actually holds (here `MAX_VALUE_LEN + 1`, close to 4 GiB).
    /// `next` must reject it before allocating a buffer sized by that declared length, never
    /// panic (capacity overflow / OOM) or actually perform a multi-gigabyte allocation.
    #[test]
    fn oversized_value_len_is_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&MAGIC);
        body.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // header_len = 0
        body.extend_from_slice(&1u64.to_le_bytes()); // record_count = 1
        body.extend_from_slice(&[3u8; 32]); // resource_id
        body.extend_from_slice(&(MAX_VALUE_LEN + 1).to_le_bytes()); // value_len over the cap

        // A trailing digest is irrelevant here: the reader must reject the oversized `value_len`
        // from `next` before it ever gets far enough to check the digest, so any 32 bytes will do.
        body.extend_from_slice(&[0u8; 32]);

        let (_hdr, mut reader) = SnapshotReader::<_, Sha256>::open(body.as_slice()).unwrap();
        let result = reader.next();
        assert!(
            matches!(result, Err(SnapshotError::Malformed(_))),
            "expected Malformed, got {result:?}"
        );
    }

    /// A record honestly declares `value_len = 100` but the stream only has 10 more bytes before
    /// EOF: the bounded reader must observe the short read and report `Truncated`, not silently
    /// yield a shorter-than-declared value.
    #[test]
    fn truncated_value_is_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&MAGIC);
        body.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // header_len = 0
        body.extend_from_slice(&1u64.to_le_bytes()); // record_count = 1
        body.extend_from_slice(&[4u8; 32]); // resource_id
        body.extend_from_slice(&100u32.to_le_bytes()); // value_len declares 100 bytes
        body.extend_from_slice(&[0xCC; 10]); // only 10 bytes actually follow

        let (_hdr, mut reader) = SnapshotReader::<_, Sha256>::open(body.as_slice()).unwrap();
        let result = reader.next();
        assert!(
            matches!(result, Err(SnapshotError::Truncated)),
            "expected Truncated, got {result:?}"
        );
    }

    /// A body whose declared `record_count` is honest (2) but whose second record is cut off
    /// before its `value_len` field: the first record must parse fine, and the second must fail
    /// exactly at the missing field rather than at the record boundary.
    #[test]
    fn truncated_mid_record_is_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&MAGIC);
        body.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // header_len = 0
        body.extend_from_slice(&2u64.to_le_bytes()); // record_count = 2

        // Record 1: full record, 4-byte value.
        body.extend_from_slice(&[1u8; 32]);
        body.extend_from_slice(&4u32.to_le_bytes());
        body.extend_from_slice(&[0xAA; 4]);

        // Record 2: only the 32-byte id is present; value_len and value are missing.
        body.extend_from_slice(&[2u8; 32]);

        let (_hdr, mut reader) = SnapshotReader::<_, Sha256>::open(body.as_slice()).unwrap();
        let first = reader.next().unwrap();
        assert!(first.is_some());

        let result = reader.next();
        assert!(
            matches!(result, Err(SnapshotError::Truncated)),
            "expected Truncated, got {result:?}"
        );
    }
}
