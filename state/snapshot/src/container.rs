use std::io::{Read, Write};

use sha2::Digest;
use vprogs_core_hashing::{Hasher, Sha256};
use vprogs_core_types::ResourceId;

/// Magic + embedded format tag. Bump the trailing digits on any breaking layout change.
pub const MAGIC: [u8; 8] = *b"VPSNAP01";
/// Container framing version. Readers reject unknown versions.
pub const FORMAT_VERSION: u16 = 1;

/// One resource's latest state: an opaque id and opaque value bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub resource_id: ResourceId,
    pub value: Vec<u8>,
}

/// Errors from reading or writing a snapshot container.
#[derive(Debug)]
pub enum SnapshotError {
    Io(std::io::Error),
    BadMagic,
    UnsupportedVersion(u16),
    DigestMismatch,
    Truncated,
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
                write!(f, "snapshot digest mismatch (corrupt or tampered)")
            }
            SnapshotError::Truncated => write!(f, "snapshot truncated"),
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

/// Frame `header` and `records` into the digest-sealed container format documented on
/// [`MAGIC`]. `record_count` must equal the number of items `records` yields; this is
/// checked with a `debug_assert` since callers control both from the same iteration.
pub fn write_container<W: Write>(
    w: &mut W,
    header: &[u8],
    record_count: u64,
    records: impl IntoIterator<Item = Record>,
) -> Result<(), SnapshotError> {
    let mut hw = HashingWriter { inner: w, hasher: sha2::Sha256::new() };

    hw.write_all(&MAGIC)?;
    hw.write_all(&FORMAT_VERSION.to_le_bytes())?;
    hw.write_all(&(header.len() as u32).to_le_bytes())?;
    hw.write_all(header)?;
    hw.write_all(&record_count.to_le_bytes())?;

    let mut written = 0u64;
    for rec in records {
        hw.write_all(rec.resource_id.as_slice())?;
        hw.write_all(&(rec.value.len() as u32).to_le_bytes())?;
        hw.write_all(&rec.value)?;
        written += 1;
    }
    debug_assert_eq!(written, record_count, "record iterator disagreed with record_count");

    let digest = hw.hasher.finalize();
    hw.inner.write_all(&digest)?;
    Ok(())
}

/// Parse and verify a container written by [`write_container`]. Rejects unknown magic,
/// unsupported format versions, a truncated stream, or a digest that does not match the
/// body (corruption or tampering).
pub fn read_container<R: Read>(r: &mut R) -> Result<(Vec<u8>, Vec<Record>), SnapshotError> {
    // Read the whole stream so we can both parse and verify the trailing digest.
    let mut all = Vec::new();
    r.read_to_end(&mut all)?;

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
    if Sha256::hash(body) != <[u8; 32]>::try_from(digest).unwrap() {
        return Err(SnapshotError::DigestMismatch);
    }

    let mut cur = std::io::Cursor::new(body);
    let mut magic = [0u8; 8];
    read_exact(&mut cur, &mut magic)?;
    let version = read_u16(&mut cur)?;
    if version != FORMAT_VERSION {
        return Err(SnapshotError::UnsupportedVersion(version));
    }
    let header_len = read_u32(&mut cur)? as usize;
    let mut header = vec![0u8; header_len];
    read_exact(&mut cur, &mut header)?;

    let record_count = read_u64(&mut cur)?;
    let mut records = Vec::with_capacity(record_count as usize);
    for _ in 0..record_count {
        let mut id = [0u8; 32];
        read_exact(&mut cur, &mut id)?;
        let value_len = read_u32(&mut cur)? as usize;
        let mut value = vec![0u8; value_len];
        read_exact(&mut cur, &mut value)?;
        records.push(Record { resource_id: ResourceId::from(id), value });
    }
    Ok((header, records))
}

fn read_exact<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<(), SnapshotError> {
    r.read_exact(buf).map_err(|_| SnapshotError::Truncated)
}
fn read_u16<R: Read>(r: &mut R) -> Result<u16, SnapshotError> {
    let mut b = [0u8; 2];
    read_exact(r, &mut b)?;
    Ok(u16::from_le_bytes(b))
}
fn read_u32<R: Read>(r: &mut R) -> Result<u32, SnapshotError> {
    let mut b = [0u8; 4];
    read_exact(r, &mut b)?;
    Ok(u32::from_le_bytes(b))
}
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
}
