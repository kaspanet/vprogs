use alloc::vec::Vec;

use borsh::{BorshDeserialize, BorshSerialize};

use crate::AccountInput;

/// Owned witness data for passing to ZK backends.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct Witness {
    pub tx_bytes: Vec<u8>,
    pub tx_index: u32,
    pub batch_metadata: Vec<u8>,
    pub accounts: Vec<AccountInput>,
}

/// Zero-copy reference to a single account input within the witness buffer.
pub struct AccountInputRef<'a> {
    pub account_id: &'a [u8],
    pub data: &'a [u8],
    pub version: u64,
}

/// Zero-copy parsed witness data for guest consumption.
pub struct WitnessRef<'a> {
    pub tx_bytes: &'a [u8],
    pub tx_index: u32,
    pub batch_metadata: &'a [u8],
    pub accounts: Vec<AccountInputRef<'a>>,
}

/// Reads the flat wire-format witness buffer without allocation (beyond the account vec).
///
/// Wire format (all integers little-endian, fields padded to 4-byte alignment):
/// ```text
/// [total_len: u32]
/// [tx_bytes_len: u32] [tx_bytes: u8*] [pad to align(4)]
/// [tx_index: u32]
/// [batch_metadata_len: u32] [batch_metadata: u8*] [pad to align(4)]
/// [num_accounts: u32]
///   for each account:
///     [account_id_len: u32] [account_id: u8*] [pad to align(4)]
///     [data_len: u32] [data: u8*] [pad to align(4)]
///     [version: u64]
/// ```
pub struct WitnessReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> WitnessReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Parses the witness buffer into a zero-copy [`WitnessRef`].
    pub fn read(mut self) -> WitnessRef<'a> {
        let _total_len = self.read_u32();

        let tx_bytes = self.read_bytes_aligned();
        let tx_index = self.read_u32();
        let batch_metadata = self.read_bytes_aligned();
        let num_accounts = self.read_u32();

        let mut accounts = Vec::with_capacity(num_accounts as usize);
        for _ in 0..num_accounts {
            let account_id = self.read_bytes_aligned();
            let data = self.read_bytes_aligned();
            let version = self.read_u64();
            accounts.push(AccountInputRef { account_id, data, version });
        }

        WitnessRef { tx_bytes, tx_index, batch_metadata, accounts }
    }

    fn read_u32(&mut self) -> u32 {
        let val = u32::from_le_bytes(self.buf[self.pos..self.pos + 4].try_into().unwrap());
        self.pos += 4;
        val
    }

    fn read_u64(&mut self) -> u64 {
        let val = u64::from_le_bytes(self.buf[self.pos..self.pos + 8].try_into().unwrap());
        self.pos += 8;
        val
    }

    fn read_bytes_aligned(&mut self) -> &'a [u8] {
        let len = self.read_u32() as usize;
        let data = &self.buf[self.pos..self.pos + len];
        self.pos += len;
        // Pad to 4-byte alignment.
        let rem = self.pos % 4;
        if rem != 0 {
            self.pos += 4 - rem;
        }
        data
    }
}
