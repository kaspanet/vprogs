```
// =======================
// Guest-side wire format
// =======================
//
// [u32 n]
// [u32 len0][u32 len1]...[u32 len(n-1)]
// [bytes account0][bytes account1]...[bytes account(n-1)]


// =======================
// Core types
// =======================

pub struct Abi;

pub struct Account<'a> {
    // Always points into the ingested working blob slice for *this* account.
    // Disjoint across all accounts.
    backing: &'a mut [u8],

    // Set on first "grow"/promotion. From then on, all reads/writes use this.
    promoted: Option<Vec<u8>>,

    // Optional but useful.
    dirty: bool,
}

impl<'a> Account<'a> {
    pub fn borrow(&self) -> &[u8] {
        match self.promoted.as_ref() {
            Some(v) => v.as_slice(),
            None => &self.backing,
        }
    }

    pub fn borrow_mut(&mut self) -> &mut [u8] {
        self.dirty = true;
        match self.promoted.as_mut() {
            Some(v) => v.as_mut_slice(),
            None => self.backing,
        }
    }

    // "Promotion" happens here (or in any other API that requires resizing).
    pub fn grow(&mut self, new_len: usize) {
        self.dirty = true;

        if self.promoted.is_none() {
            // Copy current truth (backing may already have in-place mutations)
            self.promoted = Some(self.backing.to_vec());
        }
        let v = self.promoted.as_mut().unwrap();
        v.resize(new_len, 0);
    }

    pub fn final_bytes(&self) -> &[u8] {
        self.borrow()
    }
}


// =======================
// Abi::main
// =======================

impl Abi {
    pub fn main<R>(
        input_blob: &mut [u8],
        f: impl for<'a> FnOnce(&mut [Account<'a>]) -> R,
    ) -> AbiOutput {
        // 1) parse header -> lens[]
        let (lens, payload_start) = parse_header(input_blob);

        // 2) carve payload into disjoint &mut [u8] regions (zero-copy)
        let payload = &mut input_blob[payload_start..];
        let mut accounts = provision_accounts(payload, &lens);

        // 3) run guest logic
        let _ret = f(&mut accounts);

        // 4) finalize: choose promoted if present else backing (dump all or only dirty)
        finalize(accounts)
    }
}


// =======================
// Parsing / provisioning
// =======================

fn parse_header(blob: &[u8]) -> (Vec<usize>, usize) {
    // pseudocode; do bounds checks + overflow-safe math in real code
    let mut p = 0;
    let n = read_u32_le(blob, &mut p) as usize;
    let mut lens = Vec::with_capacity(n);
    for _ in 0..n {
        lens.push(read_u32_le(blob, &mut p) as usize);
    }
    (lens, p) // p is payload start
}

fn provision_accounts<'a>(mut payload: &'a mut [u8], lens: &[usize]) -> Vec<Account<'a>> {
    let mut out = Vec::with_capacity(lens.len());
    for &len in lens {
        let (acc_bytes, rest) = payload.split_at_mut(len);
        out.push(Account {
            backing: acc_bytes,
            promoted: None,
            dirty: false,
        });
        payload = rest;
    }
    out
}


// =======================
// Finalization / output
// =======================

pub struct AbiOutput {
    // simplest: dump all account bytes in order
    pub accounts: Vec<Vec<u8>>,
    // optional: only changed indices
    pub dirty_indices: Vec<usize>,
}

fn finalize<'a>(accounts: Vec<Account<'a>>) -> AbiOutput {
    let mut out = Vec::with_capacity(accounts.len());
    let mut dirty = Vec::new();

    for (i, acc) in accounts.into_iter().enumerate() {
        if acc.dirty {
            dirty.push(i);
        }
        // dump final bytes (promoted if present else backing)
        out.push(acc.final_bytes().to_vec());
    }

    AbiOutput { accounts: out, dirty_indices: dirty }
}


// =======================
// Helpers
// =======================

fn read_u32_le(b: &[u8], p: &mut usize) -> u32 {
    let v = u32::from_le_bytes(b[*p..*p + 4].try_into().unwrap());
    *p += 4;
    v
}


// =======================
// Example guest usage
// =======================

fn guest_program(mut input: &mut [u8]) -> AbiOutput {
    Abi::main(&mut input, |accounts| {
        let bal = accounts[1].borrow_mut();
        // mutate in place...
        bal[0] ^= 1;

        // promote + grow a different account without affecting `bal`
        accounts[0].grow(1024);
        accounts[0].borrow_mut()[0] = 42;
    })
}