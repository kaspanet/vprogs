use vprogs_zk_smt::MultiProof;

use super::{
    ACCOUNT_ENTRY_SIZE, HEADER_SIZE, account_entry::AccountEntry, header::BatchWitnessHeader,
    tx_entry::TxEntryIter,
};

/// Zero-copy decoder for the batch witness format.
pub struct BatchWitnessDecoder<'a> {
    buf: &'a [u8],
    /// Offset past header + account entries + multi_proof, where tx entries start.
    tx_offset: usize,
    /// Number of accounts.
    n_accounts: u32,
    /// Number of transactions.
    n_txs: u32,
    /// Start offset of the multi-proof bytes (after the length prefix).
    mp_start: usize,
    /// Length of the multi-proof bytes.
    mp_len: usize,
}

impl<'a> BatchWitnessDecoder<'a> {
    /// Decodes the batch witness from a raw byte buffer.
    ///
    /// # Panics
    /// Panics if the buffer is truncated or malformed.
    pub fn new(buf: &'a [u8]) -> Self {
        assert!(buf.len() >= HEADER_SIZE, "batch witness too short for header");

        let n_accounts = u32::from_le_bytes(buf[72..76].try_into().expect("truncated n_accounts"));
        let n_txs = u32::from_le_bytes(buf[76..80].try_into().expect("truncated n_txs"));

        let accounts_end = HEADER_SIZE + (n_accounts as usize) * ACCOUNT_ENTRY_SIZE;
        assert!(buf.len() >= accounts_end, "batch witness too short for account entries");

        // Read multi-proof length prefix.
        let mp_len = u32::from_le_bytes(
            buf[accounts_end..accounts_end + 4].try_into().expect("truncated multi-proof length"),
        ) as usize;
        let mp_start = accounts_end + 4;
        let tx_offset = mp_start + mp_len;

        Self { buf, tx_offset, n_accounts, n_txs, mp_start, mp_len }
    }

    /// Returns the decoded header.
    pub fn header(&self) -> BatchWitnessHeader<'a> {
        BatchWitnessHeader {
            image_id: self.buf[0..32].try_into().expect("truncated image_id"),
            batch_index: u64::from_le_bytes(
                self.buf[32..40].try_into().expect("truncated batch_index"),
            ),
            prev_root: self.buf[40..72].try_into().expect("truncated prev_root"),
            n_accounts: self.n_accounts,
            n_txs: self.n_txs,
        }
    }

    /// Returns the account entry at the given index.
    pub fn account_entry(&self, index: u32) -> AccountEntry<'a> {
        let base = HEADER_SIZE + (index as usize) * ACCOUNT_ENTRY_SIZE;
        AccountEntry {
            resource_id: self.buf[base..base + 32].try_into().expect("truncated resource_id"),
            is_new: self.buf[base + 32] != 0,
            leaf_hash: self.buf[base + 33..base + 65].try_into().expect("truncated leaf_hash"),
        }
    }

    /// Returns a zero-copy multi-proof view borrowing from the witness buffer.
    pub fn multi_proof(&self) -> MultiProof<'a> {
        MultiProof::decode(&self.buf[self.mp_start..self.mp_start + self.mp_len])
    }

    /// Returns an iterator over the transaction entries.
    pub fn tx_entries(&self) -> TxEntryIter<'a> {
        TxEntryIter { buf: self.buf, offset: self.tx_offset, remaining: self.n_txs }
    }
}
