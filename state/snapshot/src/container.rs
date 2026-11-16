//! Streaming snapshot codec: an opaque header plus a stream of `(resource_id, value)` records,
//! framed with a trailing digest over everything before it.
//!
//! Wire layout (little-endian integers):
//! `MAGIC[8] | version:u16 | header_len:u32 | header[header_len] | record_count:u64 |
//! { id[32] | value_len:u32 | value[value_len] }*record_count | digest[32]`
//!
//! `MAGIC` and the format version are pinned by the caller's [`SnapshotFormat`] implementation
//! rather than hardcoded here: the container is generic over both `F: SnapshotFormat` (framing
//! identity) and `H: Hasher` (digest algorithm), so distinct callers can stamp distinct
//! magics/versions on the same codec without forking it.
//!
//! Neither side ever holds the whole file or the whole record set in memory. [`SnapshotWriter`] is
//! push-style: [`SnapshotWriter::write_record`] takes borrowed `id`/`value` slices, so a caller can
//! stream straight from e.g. a RocksDB cursor or `get_pinned` with no `Vec` in between.
//! [`SnapshotReader`] is a lending reader: [`SnapshotReader::next`] returns slices borrowed from a
//! reused internal buffer, valid until the next call, so reading never allocates per record. Both
//! sides fold the bytes they produce/consume into `H::incremental()` so the trailing digest never
//! requires a second pass over the body.

use std::{
    io::{Read, Write},
    marker::PhantomData,
};

use vprogs_core_hashing::{Hasher, IncrementalHasher};

/// Framing identity for a snapshot container: the leading magic and format version a reader
/// checks before trusting any framing. A caller pins both by implementing this on a marker type.
pub trait SnapshotFormat {
    /// Leading magic bytes identifying this format. [`SnapshotReader::open`] rejects any input
    /// whose leading 8 bytes do not match.
    const MAGIC: [u8; 8];
    /// Framing version. [`SnapshotReader::open`] rejects any input declaring a different version.
    /// Bump this on any breaking layout change.
    const FORMAT_VERSION: u16;
}

/// Cap on the opaque header region. The header carries caller metadata (small and structured),
/// never resource values, so 1 MiB is generous while still bounding the allocation
/// [`SnapshotReader::open`] performs before it has validated anything else about the file.
pub const MAX_HEADER_LEN: u32 = 1 << 20;
/// Per-record value cap, enforced by [`SnapshotReader::next`] before it allocates anything sized
/// by the untrusted on-wire `value_len`. 256 MiB is generous for a single resource blob under this
/// account/state model; a program that legitimately needs a larger single value should bump this
/// constant rather than work around it.
pub const MAX_VALUE_LEN: u32 = 256 * 1024 * 1024;

/// Errors from reading or writing a snapshot.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// I/O failure while reading or writing the snapshot.
    #[error("snapshot io error: {0}")]
    Io(#[from] std::io::Error),
    /// Input's leading bytes do not match the reader's [`SnapshotFormat::MAGIC`]; not a snapshot
    /// this reader's format recognizes.
    #[error("not a recognized snapshot format (bad magic)")]
    BadMagic,
    /// Body declares a format version other than the reader's [`SnapshotFormat::FORMAT_VERSION`].
    #[error("unsupported snapshot format version {0}")]
    UnsupportedVersion(u16),
    /// Trailing digest does not match the body (corrupted in transit or at rest).
    #[error("snapshot digest mismatch (corrupted in transit or at rest)")]
    DigestMismatch,
    /// Stream ended before a fixed-size or declared-length field could be fully read.
    #[error("snapshot truncated")]
    Truncated,
    /// A field or call this codec refuses on its face, independent of how many bytes the stream
    /// actually holds: a length-prefixed field over its cap (`header_len > MAX_HEADER_LEN`,
    /// `value_len > MAX_VALUE_LEN`) on either side, records not in strictly ascending id order
    /// on either side, a [`SnapshotWriter::finish`] whose written count disagrees with the
    /// declared one, or a structurally invalid [`SnapshotReader::finish`] call (early finish,
    /// or trailing bytes after the digest).
    #[error("snapshot malformed: {0}")]
    Malformed(&'static str),
    /// [`SnapshotWriter::open`] or [`SnapshotWriter::write_record`] was asked to frame a header or
    /// value whose length does not fit in the on-wire `u32` field.
    #[error("snapshot field exceeds the on-wire u32 length limit")]
    FieldTooLarge,
}

/// Forwards writes to `inner`, folding exactly the bytes `inner` accepts into `hasher`.
struct HashingWriter<'a, W: Write, Inc: IncrementalHasher> {
    /// Sink every write is forwarded to.
    inner: &'a mut W,
    /// Running digest over the bytes actually forwarded so far.
    hasher: Inc,
}

impl<W: Write, Inc: IncrementalHasher> Write for HashingWriter<'_, W, Inc> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        // Fold only the bytes actually accepted: on a short write, `write_all`'s retry loop
        // re-offers the unwritten remainder, so folding `n` (not the whole slice) stays correct.
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Push-style writer for a snapshot: the caller opens it once, calls
/// [`write_record`](Self::write_record) for each record in strictly ascending `id` order,
/// then [`finish`](Self::finish). Every write goes straight through to the underlying `W`
/// with no intermediate `Vec`, so a caller can stream borrowed slices (e.g. a RocksDB
/// cursor's key and `get_pinned` value) straight to the file.
///
/// Generic over `H` (the digest algorithm folded over every byte written) and `F` (the framing
/// identity: [`SnapshotFormat::MAGIC`] and [`SnapshotFormat::FORMAT_VERSION`]).
pub struct SnapshotWriter<'w, W: Write, H: Hasher, F: SnapshotFormat> {
    /// Sink for every byte written, folding it into the running digest.
    hw: HashingWriter<'w, W, H::Incremental>,
    /// Previously written id, enforcing strictly ascending order.
    prev_id: Option<[u8; 32]>,
    /// Records written so far via [`write_record`](Self::write_record).
    written: u64,
    /// `record_count` declared at [`open`](Self::open) time.
    expected: u64,
    /// Binds the framing identity without storing a value.
    _format: PhantomData<F>,
}

impl<'w, W: Write, H: Hasher, F: SnapshotFormat> SnapshotWriter<'w, W, H, F> {
    /// Writes the fixed prefix (`F::MAGIC`, `F::FORMAT_VERSION`, `header_len`, `header`,
    /// `record_count`) and starts the running digest over everything written from here on.
    ///
    /// `header` is opaque; interpreting it is the caller's responsibility (e.g. the runner's
    /// encoded typed header). Returns [`SnapshotError::FieldTooLarge`] if `header` is longer
    /// than `u32::MAX`, rather than silently truncating the on-wire length prefix, and
    /// [`SnapshotError::Malformed`] if it exceeds [`MAX_HEADER_LEN`], matching the reader's
    /// cap.
    pub fn open(w: &'w mut W, header: &[u8], record_count: u64) -> Result<Self, SnapshotError> {
        let header_len: u32 = header.len().try_into().map_err(|_| SnapshotError::FieldTooLarge)?;
        if header.len() > MAX_HEADER_LEN as usize {
            return Err(SnapshotError::Malformed("header exceeds MAX_HEADER_LEN"));
        }

        let mut hw = HashingWriter::<_, H::Incremental> { inner: w, hasher: H::incremental() };
        hw.write_all(&F::MAGIC)?;
        hw.write_all(&F::FORMAT_VERSION.to_le_bytes())?;
        hw.write_all(&header_len.to_le_bytes())?;
        hw.write_all(header)?;
        hw.write_all(&record_count.to_le_bytes())?;

        Ok(Self { hw, prev_id: None, written: 0, expected: record_count, _format: PhantomData })
    }

    /// Appends one record (`id[32] | value_len:u32 | value`), folding it into the running digest.
    ///
    /// `id` MUST be strictly ascending across calls: the canonical on-wire order, and the order
    /// the reader enforces on the way in. A duplicate or descending id is rejected with
    /// [`SnapshotError::Malformed`] in every build profile rather than shipped in a
    /// digest-valid file that a restore rejects.
    ///
    /// Returns [`SnapshotError::FieldTooLarge`] if `value` is longer than `u32::MAX`, rather
    /// than silently truncating the on-wire length prefix, and [`SnapshotError::Malformed`] if
    /// it exceeds [`MAX_VALUE_LEN`], matching the reader's cap.
    pub fn write_record(&mut self, id: &[u8; 32], value: &[u8]) -> Result<(), SnapshotError> {
        let value_len: u32 = value.len().try_into().map_err(|_| SnapshotError::FieldTooLarge)?;
        if value.len() > MAX_VALUE_LEN as usize {
            return Err(SnapshotError::Malformed("record value exceeds MAX_VALUE_LEN"));
        }

        if let Some(prev) = self.prev_id {
            if prev >= *id {
                return Err(SnapshotError::Malformed("records not sorted by resource_id"));
            }
        }
        self.prev_id = Some(*id);

        self.hw.write_all(id)?;
        self.hw.write_all(&value_len.to_le_bytes())?;
        self.hw.write_all(value)?;
        self.written += 1;
        Ok(())
    }

    /// Writes the trailing digest over every byte written since [`open`](Self::open).
    ///
    /// Returns [`SnapshotError::Malformed`] in every build profile if fewer records were
    /// written than declared at open: a digest-valid but count-wrong snapshot surfaces only at
    /// restore time, misdiagnosed as corruption.
    pub fn finish(self) -> Result<(), SnapshotError> {
        if self.written != self.expected {
            return Err(SnapshotError::Malformed(
                "write_record call count disagreed with record_count declared at open",
            ));
        }
        let HashingWriter { inner, hasher } = self.hw;
        inner.write_all(&hasher.finalize())?;
        Ok(())
    }
}

/// A borrowed record from a snapshot: a resource id and its value bytes, valid until the next read.
#[derive(Clone, Copy, Debug)]
pub struct Record<'a> {
    /// The record's resource id.
    pub id: &'a [u8; 32],
    /// The record's value bytes.
    pub value: &'a [u8],
}

/// Streaming, lending reader over a snapshot produced by [`SnapshotWriter`]. The whole file is
/// never buffered: [`open`](Self::open) reads and validates only the fixed prefix,
/// [`next`](Self::next) reads one record at a time into reused buffers, and
/// [`finish`](Self::finish) checks the trailing digest once all records have been consumed.
pub struct SnapshotReader<R: Read, H: Hasher, F: SnapshotFormat> {
    /// Underlying byte stream, positioned just past the last byte consumed.
    reader: R,
    /// Running digest over every byte read so far, magic through the last record.
    hasher: H::Incremental,
    /// `record_count` declared by the header, fixed at [`open`](Self::open) time.
    record_count: u64,
    /// Records not yet yielded by [`next`](Self::next).
    remaining: u64,
    /// Reused buffer for the current record's id, overwritten by every [`next`](Self::next) call.
    id_buf: [u8; 32],
    /// Reused buffer for the current record's value, cleared and refilled by every
    /// [`next`](Self::next) call; never preallocated to the declared `value_len`.
    value_buf: Vec<u8>,
    /// Previously yielded id, enforcing strictly ascending order.
    prev_id: Option<[u8; 32]>,
    /// Binds the framing identity without storing a value.
    _format: PhantomData<F>,
}

impl<R: Read, H: Hasher, F: SnapshotFormat> SnapshotReader<R, H, F> {
    /// Reads and validates the fixed prefix (`magic`, `version`, `header_len`, `header`,
    /// `record_count`) and returns the opaque header. Rejects unknown magic
    /// ([`SnapshotFormat::MAGIC`]) or an unsupported version ([`SnapshotFormat::FORMAT_VERSION`])
    /// before ever looking at `header_len`, and rejects a `header_len` over [`MAX_HEADER_LEN`]
    /// before allocating the header buffer.
    pub fn open(mut r: R) -> Result<(Vec<u8>, Self), SnapshotError> {
        let mut hasher = H::incremental();

        let mut magic = [0u8; 8];
        read_exact_fold(&mut r, &mut magic, &mut hasher)?;
        if magic != F::MAGIC {
            return Err(SnapshotError::BadMagic);
        }

        let version = read_u16_fold(&mut r, &mut hasher)?;
        if version != F::FORMAT_VERSION {
            return Err(SnapshotError::UnsupportedVersion(version));
        }

        let header_len = read_u32_fold(&mut r, &mut hasher)?;
        if header_len > MAX_HEADER_LEN {
            return Err(SnapshotError::Malformed("header_len exceeds MAX_HEADER_LEN"));
        }
        let mut header = vec![0u8; header_len as usize];
        read_exact_fold(&mut r, &mut header, &mut hasher)?;

        let record_count = read_u64_fold(&mut r, &mut hasher)?;

        Ok((
            header,
            SnapshotReader {
                reader: r,
                hasher,
                record_count,
                remaining: record_count,
                id_buf: [0u8; 32],
                value_buf: Vec::new(),
                prev_id: None,
                _format: PhantomData,
            },
        ))
    }

    /// The `record_count` declared by the header, fixed at [`open`](Self::open) time.
    pub fn record_count(&self) -> u64 {
        self.record_count
    }

    /// Reads the next record into the reused internal buffers and returns borrowed `(id, value)`,
    /// valid until the next call to `next` (or until `self` is dropped). Returns `Ok(None)` once
    /// all `record_count` records have been consumed.
    ///
    /// Rejects a `value_len` over [`MAX_VALUE_LEN`] with [`SnapshotError::Malformed`] before
    /// allocating anything sized by it, and never pre-allocates the declared length: a `value_len`
    /// the stream cannot back yields [`SnapshotError::Truncated`], not a buffer pre-sized to a
    /// length the file never delivers.
    ///
    /// Records must arrive in strictly ascending `id` order (the canonical on-wire order; the
    /// writer enforces the same). A snapshot file is untrusted input and ordering is a framing
    /// invariant the trailing digest cannot repair (whoever reorders the body recomputes the
    /// digest just as easily), so a non-ascending or duplicate id is rejected with
    /// [`SnapshotError::Malformed`] at the record boundary, before the value is read and before
    /// a restore can act on the records.
    // Named `next`, not an `Iterator` impl: an `Iterator` cannot return a value borrowed from
    // `&mut self` (a lending iterator), so an `Iterator<Item = Result<..>>` here would force an
    // owned allocation per record, defeating the per-record streaming this type exists for. The
    // lint fires on the name alone and is a false positive for that deliberate shape.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Record<'_>>, SnapshotError> {
        if self.remaining == 0 {
            return Ok(None);
        }

        read_exact_fold(&mut self.reader, &mut self.id_buf, &mut self.hasher)?;
        if let Some(prev) = self.prev_id {
            if self.id_buf <= prev {
                return Err(SnapshotError::Malformed("records not sorted by resource_id"));
            }
        }
        self.prev_id = Some(self.id_buf);

        let value_len = read_u32_fold(&mut self.reader, &mut self.hasher)?;
        if value_len > MAX_VALUE_LEN {
            return Err(SnapshotError::Malformed("record value_len exceeds MAX_VALUE_LEN"));
        }
        // Bounded by the `take` limit, not by `value_len` up front: the reused buffer only ever
        // grows to as large as the bytes actually read, so a hostile `value_len` within the cap
        // still can't force an allocation the stream doesn't back.
        self.value_buf.clear();
        let n = self.reader.by_ref().take(value_len as u64).read_to_end(&mut self.value_buf)?;
        if n as u64 != value_len as u64 {
            return Err(SnapshotError::Truncated);
        }
        self.hasher.update(&self.value_buf);

        self.remaining -= 1;
        Ok(Some(Record { id: &self.id_buf, value: &self.value_buf }))
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
        // The digest itself is not folded into the running hash: it covers exactly the bytes
        // before it.
        reader.read_exact(&mut digest).map_err(|e| match e.kind() {
            std::io::ErrorKind::UnexpectedEof => SnapshotError::Truncated,
            _ => SnapshotError::Io(e),
        })?;
        if self.hasher.finalize() != digest {
            return Err(SnapshotError::DigestMismatch);
        }

        let mut probe = [0u8; 1];
        loop {
            match reader.read(&mut probe) {
                Ok(0) => break,
                Ok(_) => return Err(SnapshotError::Malformed("trailing bytes after digest")),
                // A bare `read` must retry EINTR itself, like `read_exact`/`read_to_end` do.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(SnapshotError::Io(e)),
            }
        }
        Ok(())
    }
}

/// Fills `buf` completely and folds the bytes read into `hasher`. Only a genuine end of stream
/// maps to [`SnapshotError::Truncated`]; any other I/O failure propagates as
/// [`SnapshotError::Io`] so a failing disk is not misdiagnosed as a corrupt file.
fn read_exact_fold<R: Read, Inc: IncrementalHasher>(
    r: &mut R,
    buf: &mut [u8],
    hasher: &mut Inc,
) -> Result<(), SnapshotError> {
    r.read_exact(buf).map_err(|e| match e.kind() {
        std::io::ErrorKind::UnexpectedEof => SnapshotError::Truncated,
        _ => SnapshotError::Io(e),
    })?;
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

    use super::*;

    /// Test-only framing identity: an arbitrary magic distinct from any real caller's, pinned to
    /// format version 1.
    struct TestFormat;

    impl SnapshotFormat for TestFormat {
        const MAGIC: [u8; 8] = *b"TESTSNP0";
        const FORMAT_VERSION: u16 = 1;
    }

    #[test]
    fn streaming_round_trip_via_next() {
        let header = b"opaque-header-bytes".to_vec();
        let records: Vec<([u8; 32], Vec<u8>)> = vec![
            ([1u8; 32], b"alpha".to_vec()),
            ([2u8; 32], b"".to_vec()),
            ([3u8; 32], b"gamma-value".to_vec()),
        ];

        let mut buf = Vec::new();
        let mut writer =
            SnapshotWriter::<_, Sha256, TestFormat>::open(&mut buf, &header, records.len() as u64)
                .unwrap();
        for (id, value) in &records {
            writer.write_record(id, value).unwrap();
        }
        writer.finish().unwrap();

        let (got_header, mut reader) =
            SnapshotReader::<_, Sha256, TestFormat>::open(buf.as_slice()).unwrap();
        assert_eq!(got_header, header);
        assert_eq!(reader.record_count(), records.len() as u64);

        let mut got: Vec<([u8; 32], Vec<u8>)> = Vec::new();
        while let Some(Record { id, value }) = reader.next().unwrap() {
            got.push((*id, value.to_vec()));
        }
        reader.finish().unwrap();
        assert_eq!(got, records);
    }

    /// Records must be written in strictly ascending `id` order: the canonical on-wire order
    /// and the order the restore path consumes. A descending id is rejected with an error in
    /// every build profile, not merely tripped by a debug assertion.
    #[test]
    fn writer_rejects_unsorted_records() {
        let mut buf = Vec::new();
        let mut writer = SnapshotWriter::<_, Sha256, TestFormat>::open(&mut buf, b"h", 2).unwrap();
        writer.write_record(&[2u8; 32], b"beta").unwrap();
        assert!(matches!(
            writer.write_record(&[1u8; 32], b"alpha"),
            Err(SnapshotError::Malformed("records not sorted by resource_id"))
        ));
    }

    /// A duplicate id re-fed on an exporter retry/resume satisfies a merely non-decreasing
    /// check but still breaks strictly-ascending consumers; the writer must reject it like any
    /// other ordering violation, in every build profile.
    #[test]
    fn writer_rejects_duplicate_records() {
        let mut buf = Vec::new();
        let mut writer = SnapshotWriter::<_, Sha256, TestFormat>::open(&mut buf, b"h", 2).unwrap();
        writer.write_record(&[1u8; 32], b"once").unwrap();
        assert!(matches!(
            writer.write_record(&[1u8; 32], b"again"),
            Err(SnapshotError::Malformed("records not sorted by resource_id"))
        ));
    }

    /// The writer enforces the reader's caps symmetrically: a header over [`MAX_HEADER_LEN`]
    /// is rejected at open time, not discovered at restore time when the snapshot may be the
    /// only surviving copy of the state.
    #[test]
    fn writer_rejects_oversized_header() {
        let mut buf = Vec::new();
        let header = vec![0u8; MAX_HEADER_LEN as usize + 1];
        let result =
            SnapshotWriter::<_, Sha256, TestFormat>::open(&mut buf, &header, 0).map(|_| ());
        assert!(
            matches!(result, Err(SnapshotError::Malformed(_))),
            "expected Malformed, got {result:?}"
        );
    }

    /// Same symmetry for values: a value over [`MAX_VALUE_LEN`] is rejected by `write_record`
    /// rather than shipped in a digest-valid file that every later restore rejects.
    #[test]
    fn writer_rejects_oversized_value() {
        let mut buf = Vec::new();
        let mut writer = SnapshotWriter::<_, Sha256, TestFormat>::open(&mut buf, b"h", 1).unwrap();
        let value = vec![0u8; MAX_VALUE_LEN as usize + 1];
        assert!(
            matches!(writer.write_record(&[1u8; 32], &value), Err(SnapshotError::Malformed(_))),
            "expected Malformed"
        );
    }

    /// A producer bug that writes fewer records than declared at open must fail `finish` in
    /// every build profile: a digest-valid but count-wrong snapshot surfaces only at restore,
    /// misdiagnosed as `Truncated` or `DigestMismatch` corruption.
    #[test]
    fn finish_rejects_written_count_below_declared() {
        let mut buf = Vec::new();
        let mut writer = SnapshotWriter::<_, Sha256, TestFormat>::open(&mut buf, b"h", 3).unwrap();
        writer.write_record(&[1u8; 32], b"one").unwrap();
        assert!(matches!(writer.finish(), Err(SnapshotError::Malformed(_))), "expected Malformed");
    }

    /// A file with a correct digest but extra bytes appended after it must not silently
    /// "verify": `finish` should notice the underlying reader isn't at EOF once the digest has
    /// been consumed and reject the trailing junk instead.
    #[test]
    fn trailing_bytes_after_digest_are_rejected() {
        let mut buf = Vec::new();
        let mut writer = SnapshotWriter::<_, Sha256, TestFormat>::open(&mut buf, b"h", 1).unwrap();
        writer.write_record(&[9u8; 32], b"x").unwrap();
        writer.finish().unwrap();
        buf.extend_from_slice(b"junk-appended-after-digest");

        let (_hdr, mut reader) =
            SnapshotReader::<_, Sha256, TestFormat>::open(buf.as_slice()).unwrap();
        while reader.next().unwrap().is_some() {}
        assert!(matches!(reader.finish(), Err(SnapshotError::Malformed(_))));
    }

    #[test]
    fn corrupted_digest_is_rejected() {
        let mut buf = Vec::new();
        let mut writer = SnapshotWriter::<_, Sha256, TestFormat>::open(&mut buf, b"h", 1).unwrap();
        writer.write_record(&[9u8; 32], b"x").unwrap();
        writer.finish().unwrap();
        let last = buf.len() - 1;
        buf[last] ^= 0xff; // flip a digest byte

        let (_hdr, mut reader) =
            SnapshotReader::<_, Sha256, TestFormat>::open(buf.as_slice()).unwrap();
        while reader.next().unwrap().is_some() {}
        assert!(matches!(reader.finish(), Err(SnapshotError::DigestMismatch)));
    }

    #[test]
    fn bad_magic_is_rejected() {
        let buf = vec![0u8; 8];
        assert!(matches!(
            SnapshotReader::<_, Sha256, TestFormat>::open(buf.as_slice()),
            Err(SnapshotError::BadMagic)
        ));
    }

    #[test]
    fn long_foreign_file_is_bad_magic_not_digest_mismatch() {
        let buf = vec![0xABu8; 200];
        assert!(matches!(
            SnapshotReader::<_, Sha256, TestFormat>::open(buf.as_slice()),
            Err(SnapshotError::BadMagic)
        ));
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&TestFormat::MAGIC);
        body.extend_from_slice(&999u16.to_le_bytes()); // unknown version

        let result = SnapshotReader::<_, Sha256, TestFormat>::open(body.as_slice());
        assert!(matches!(result, Err(SnapshotError::UnsupportedVersion(999))));
    }

    #[test]
    fn oversized_header_len_is_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&TestFormat::MAGIC);
        body.extend_from_slice(&TestFormat::FORMAT_VERSION.to_le_bytes());
        body.extend_from_slice(&(MAX_HEADER_LEN + 1).to_le_bytes());

        // Discard the `Ok` payload before formatting: `SnapshotReader` need not be `Debug`.
        let result = SnapshotReader::<_, Sha256, TestFormat>::open(body.as_slice()).map(|_| ());
        assert!(
            matches!(result, Err(SnapshotError::Malformed(_))),
            "expected Malformed, got {result:?}"
        );
    }

    /// A forged file can declare an absurd `record_count` (here `u64::MAX`) while still being
    /// tiny. The reader never preallocates
    /// anything sized by `record_count`; the first `next()` call simply runs out of stream while
    /// reading the first record's `id` and must return an error, never panic (capacity overflow)
    /// or abort (alloc failure).
    #[test]
    fn oversized_record_count_is_rejected_without_panic() {
        let mut body = Vec::new();
        body.extend_from_slice(&TestFormat::MAGIC);
        body.extend_from_slice(&TestFormat::FORMAT_VERSION.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // header_len = 0
        body.extend_from_slice(&u64::MAX.to_le_bytes()); // record_count = u64::MAX

        let (_hdr, mut reader) =
            SnapshotReader::<_, Sha256, TestFormat>::open(body.as_slice()).unwrap();
        let result = reader.next();
        assert!(
            matches!(result, Err(SnapshotError::Truncated)),
            "expected Truncated, got {result:?}"
        );
    }

    /// A forged record can declare a `value_len` far beyond anything the tiny file actually holds
    /// (here `MAX_VALUE_LEN + 1`, close to 4 GiB).
    /// `next` must reject it before allocating a buffer sized by that declared length, never
    /// panic (capacity overflow / OOM) or actually perform a multi-gigabyte allocation.
    #[test]
    fn oversized_value_len_is_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&TestFormat::MAGIC);
        body.extend_from_slice(&TestFormat::FORMAT_VERSION.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // header_len = 0
        body.extend_from_slice(&1u64.to_le_bytes()); // record_count = 1
        body.extend_from_slice(&[3u8; 32]); // resource_id
        body.extend_from_slice(&(MAX_VALUE_LEN + 1).to_le_bytes()); // value_len over the cap

        // A trailing digest is irrelevant here: the reader must reject the oversized `value_len`
        // from `next` before it ever gets far enough to check the digest, so any 32 bytes will do.
        body.extend_from_slice(&[0u8; 32]);

        let (_hdr, mut reader) =
            SnapshotReader::<_, Sha256, TestFormat>::open(body.as_slice()).unwrap();
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
        body.extend_from_slice(&TestFormat::MAGIC);
        body.extend_from_slice(&TestFormat::FORMAT_VERSION.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // header_len = 0
        body.extend_from_slice(&1u64.to_le_bytes()); // record_count = 1
        body.extend_from_slice(&[4u8; 32]); // resource_id
        body.extend_from_slice(&100u32.to_le_bytes()); // value_len declares 100 bytes
        body.extend_from_slice(&[0xCC; 10]); // only 10 bytes actually follow

        let (_hdr, mut reader) =
            SnapshotReader::<_, Sha256, TestFormat>::open(body.as_slice()).unwrap();
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
        body.extend_from_slice(&TestFormat::MAGIC);
        body.extend_from_slice(&TestFormat::FORMAT_VERSION.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // header_len = 0
        body.extend_from_slice(&2u64.to_le_bytes()); // record_count = 2

        // Record 1: full record, 4-byte value.
        body.extend_from_slice(&[1u8; 32]);
        body.extend_from_slice(&4u32.to_le_bytes());
        body.extend_from_slice(&[0xAA; 4]);

        // Record 2: only the 32-byte id is present; value_len and value are missing.
        body.extend_from_slice(&[2u8; 32]);

        let (_hdr, mut reader) =
            SnapshotReader::<_, Sha256, TestFormat>::open(body.as_slice()).unwrap();
        let first = reader.next().unwrap();
        assert!(first.is_some());

        let result = reader.next();
        assert!(
            matches!(result, Err(SnapshotError::Truncated)),
            "expected Truncated, got {result:?}"
        );
    }

    // -- Reader-side ordering and I/O-diagnosis coverage --

    /// Frames `records` in the given order with a correct trailing digest, bypassing the writer
    /// so a digest-valid file can probe reader-side validation.
    fn digest_valid_body(records: &[([u8; 32], &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&TestFormat::MAGIC);
        body.extend_from_slice(&TestFormat::FORMAT_VERSION.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // header_len = 0
        body.extend_from_slice(&(records.len() as u64).to_le_bytes());

        let mut hasher = Sha256::incremental();
        hasher.update(&body);
        for (id, value) in records {
            let value_len = value.len() as u32;
            body.extend_from_slice(id);
            body.extend_from_slice(&value_len.to_le_bytes());
            body.extend_from_slice(value);
            hasher.update(id);
            hasher.update(&value_len.to_le_bytes());
            hasher.update(value);
        }
        body.extend_from_slice(&hasher.finalize());
        body
    }

    /// An unsorted snapshot whose digest is otherwise perfectly valid: the trailing digest
    /// cannot catch it (whoever reorders the body recomputes the digest just as easily), so the
    /// reader itself must reject the non-ascending id at the record boundary, before the value
    /// is read and before a restore can persist garbage nodes.
    #[test]
    fn reader_rejects_digest_valid_unsorted_records() {
        let body = digest_valid_body(&[([3u8; 32], b"three"), ([1u8; 32], b"one")]);
        let (_hdr, mut reader) =
            SnapshotReader::<_, Sha256, TestFormat>::open(body.as_slice()).unwrap();
        assert!(reader.next().unwrap().is_some());
        assert!(matches!(
            reader.next(),
            Err(SnapshotError::Malformed("records not sorted by resource_id"))
        ));
    }

    /// The duplicate-id variant of the same forgery: the order contract is strictly ascending,
    /// not non-decreasing.
    #[test]
    fn reader_rejects_digest_valid_duplicate_ids() {
        let body = digest_valid_body(&[([1u8; 32], b"one"), ([1u8; 32], b"again")]);
        let (_hdr, mut reader) =
            SnapshotReader::<_, Sha256, TestFormat>::open(body.as_slice()).unwrap();
        assert!(reader.next().unwrap().is_some());
        assert!(matches!(
            reader.next(),
            Err(SnapshotError::Malformed("records not sorted by resource_id"))
        ));
    }

    /// Yields its bytes, then fails forever with a non-EOF I/O error: a failing disk under an
    /// otherwise good snapshot.
    struct FailingRead {
        good: Vec<u8>,
        pos: usize,
    }

    impl FailingRead {
        fn new(good: Vec<u8>) -> Self {
            Self { good, pos: 0 }
        }
    }

    impl Read for FailingRead {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.good.len() {
                return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "failing disk"));
            }
            let n = buf.len().min(self.good.len() - self.pos);
            buf[..n].copy_from_slice(&self.good[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    /// A genuine I/O failure while reading the fixed prefix must surface as `Io`, not be
    /// misdiagnosed as `Truncated` (which sends an operator re-downloading a good snapshot
    /// instead of diagnosing the disk).
    #[test]
    fn open_maps_io_error_to_io_not_truncated() {
        let good = digest_valid_body(&[]);
        let mut failing = FailingRead::new(good[..4].to_vec());
        let result = SnapshotReader::<_, Sha256, TestFormat>::open(&mut failing).map(|_| ());
        assert!(matches!(result, Err(SnapshotError::Io(_))), "expected Io, got {result:?}");
    }

    /// Same requirement on the per-record path: a stream failing mid-record yields `Io`.
    #[test]
    fn next_maps_io_error_to_io_not_truncated() {
        // A valid two-record snapshot cut right before the second record's id: the first record
        // parses, then the stream fails mid-read.
        let full = digest_valid_body(&[([1u8; 32], b"one"), ([2u8; 32], b"two")]);
        let cut = full.len() - 32 /* digest */ - 32 - 4 - 3 /* record 2 */;
        let mut failing = FailingRead::new(full[..cut].to_vec());
        let (_hdr, mut reader) =
            SnapshotReader::<_, Sha256, TestFormat>::open(&mut failing).unwrap();
        assert!(reader.next().unwrap().is_some());
        let result = reader.next();
        assert!(matches!(result, Err(SnapshotError::Io(_))), "expected Io, got {result:?}");
    }

    /// Same requirement on the digest read in `finish`.
    #[test]
    fn finish_maps_io_error_to_io_not_truncated() {
        let full = digest_valid_body(&[([1u8; 32], b"one")]);
        let cut = full.len() - 32; // everything but the digest
        let mut failing = FailingRead::new(full[..cut].to_vec());
        let (_hdr, mut reader) =
            SnapshotReader::<_, Sha256, TestFormat>::open(&mut failing).unwrap();
        while reader.next().unwrap().is_some() {}
        let result = reader.finish();
        assert!(matches!(result, Err(SnapshotError::Io(_))), "expected Io, got {result:?}");
    }

    /// Yields its bytes, then reports `Interrupted` exactly once before a clean EOF.
    struct InterruptedAtEof {
        good: Vec<u8>,
        pos: usize,
        interrupted: bool,
    }

    impl InterruptedAtEof {
        fn new(good: Vec<u8>) -> Self {
            Self { good, pos: 0, interrupted: false }
        }
    }

    impl Read for InterruptedAtEof {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.good.len() {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "signal"));
                }
                return Ok(0);
            }
            let n = buf.len().min(self.good.len() - self.pos);
            buf[..n].copy_from_slice(&self.good[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    /// The trailing-byte probe is a bare `read`, which unlike `read_exact` does not retry
    /// `Interrupted` internally: a single EINTR at EOF must not fail verification of an
    /// otherwise valid snapshot.
    #[test]
    fn finish_retries_interrupted_trailing_probe() {
        let body = digest_valid_body(&[([1u8; 32], b"one")]);
        let reader = InterruptedAtEof::new(body);
        let (_hdr, mut reader) = SnapshotReader::<_, Sha256, TestFormat>::open(reader).unwrap();
        while reader.next().unwrap().is_some() {}
        reader.finish().unwrap();
    }
}
