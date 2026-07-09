//! Parsers over a Kaspa V1 `rest_preimage` (the bytes hashed under
//! `TransactionRest` to produce a transaction's `rest_digest`).
//!
//! Layout (per `consensus/core/src/hashing/tx.rs::write_transaction` with
//! `EXCLUDE_PAYLOAD | EXCLUDE_SIGNATURE_SCRIPT | EXCLUDE_MASS_COMMIT`):
//!
//! ```text
//! u16        version
//! u64        n_inputs
//! inputs[n]:
//!   [u8;32]  prev_tx_id
//!   u32      prev_index
//!   u64      sig_script_len    // always 0 with EXCLUDE_SIGNATURE_SCRIPT
//!   bytes    sig_script[0]
//!   u64      sequence
//! u64        n_outputs
//! outputs[n]:
//!   u64      value
//!   u16      spk_version
//!   u64      spk_len
//!   bytes    spk[spk_len]
//!   (V1):
//!     u8      has_covenant
//!     (if 1): u16 auth_input | [u8;32] covenant_id
//! u64        lock_time
//! [u8;20]    subnetwork_id
//! u64        gas
//! u64        0                 // empty payload len with EXCLUDE_PAYLOAD
//! ```

use vprogs_core_codec::{Error, Reader, Result as CodecResult};

const SUBNETWORK_ID_LEN: usize = 20;

/// Skips one input entry (does not allocate).
fn skip_input(buf: &mut &[u8]) -> CodecResult<()> {
    buf.skip(32, "input.prev_tx_id")?;
    buf.skip(4, "input.prev_index")?;
    let sig_script_len = buf.le_u64("input.sig_script_len")? as usize;
    buf.skip(sig_script_len, "input.sig_script")?;
    buf.skip(8, "input.sequence")?;
    Ok(())
}

/// Returns the `(prev_tx_id, prev_index)` outpoint at `idx` in the current
/// transaction's V1 `rest_preimage`. Errors if `idx` is out of range or the
/// preimage truncates.
pub fn parse_input_outpoint_at(rest_preimage: &[u8], idx: u32) -> CodecResult<(&[u8; 32], u32)> {
    let mut buf = rest_preimage;
    buf.skip(2, "tx.version")?;
    let n_inputs = buf.le_u64("tx.n_inputs")?;
    if (idx as u64) >= n_inputs {
        return Err(Error::Decode("input_idx out of range"));
    }
    for _ in 0..idx {
        skip_input(&mut buf)?;
    }
    let prev_tx_id = buf.array::<32>("input.prev_tx_id")?;
    let prev_index = buf.le_u32("input.prev_index")?;
    Ok((prev_tx_id, prev_index))
}

/// Output extracted from a V1 `rest_preimage`.
pub struct OutputData<'a> {
    pub value: u64,
    pub spk_version: u16,
    pub spk: &'a [u8],
}

/// Walks a V1 `rest_preimage` and returns the output at `output_index`.
///
/// Currently only `tx_version == 1` is supported (V1 outputs include the
/// `has_covenant` byte and optional covenant tail).
pub fn parse_output_at_index_v1(
    rest_preimage: &[u8],
    output_index: u32,
) -> CodecResult<OutputData<'_>> {
    let mut buf = rest_preimage;
    let version = buf.le_u16("tx.version")?;
    if version != 1 {
        return Err(Error::Decode("rest_preimage: unsupported tx version"));
    }
    let n_inputs = buf.le_u64("tx.n_inputs")?;
    for _ in 0..n_inputs {
        skip_input(&mut buf)?;
    }
    let n_outputs = buf.le_u64("tx.n_outputs")?;
    if (output_index as u64) >= n_outputs {
        return Err(Error::Decode("output_index out of range"));
    }
    for i in 0..=output_index {
        let value = buf.le_u64("output.value")?;
        let spk_version = buf.le_u16("output.spk_version")?;
        // Kaspa's `write_var_bytes` (consensus/core/src/hashing/mod.rs) emits
        // a u64 length prefix, not u32, so we can't use `blob` here, which
        // reads u32. Length is bounded by tx size; cast is safe on 32/64-bit
        // hosts because the slice length below is checked against `buf.len()`
        // inside `bytes`.
        let spk_len = buf.le_u64("output.spk_len")? as usize;
        let spk = buf.bytes(spk_len, "output.spk")?;
        let has_covenant = buf.byte("output.has_covenant")?;
        if has_covenant != 0 {
            buf.skip(2, "output.auth_input")?;
            buf.skip(32, "output.covenant_id")?;
        }
        if i == output_index {
            return Ok(OutputData { value, spk_version, spk });
        }
    }
    // Loop body always returns on `i == output_index`; this path is
    // unreachable in correct control flow. Return a defensive error rather
    // than panicking; keeps the no-panic invariant in the guest.
    Err(Error::Decode("rest_preimage: output loop fell through"))
}

/// Skips the trailing fields after outputs (lock_time, subnetwork_id, gas,
/// empty payload). Exposed for tests / cross-checks; the auth path doesn't
/// need it once the target output has been read.
#[allow(dead_code)]
pub fn skip_tail(buf: &mut &[u8]) -> CodecResult<()> {
    buf.skip(8, "tx.lock_time")?;
    buf.skip(SUBNETWORK_ID_LEN, "tx.subnetwork_id")?;
    buf.skip(8, "tx.gas")?;
    let payload_len = buf.le_u64("tx.payload_len")? as usize;
    buf.skip(payload_len, "tx.payload")?;
    Ok(())
}
