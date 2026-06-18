/// On-chain script bytes for a `StandardSpk`. Variants own a fixed array sized to the script kind.
#[derive(Clone, Copy, Debug)]
pub enum ScriptBytes {
    /// 34-byte P2PK Schnorr script.
    Len34([u8; 34]),
    /// 35-byte P2PK ECDSA or P2SH script.
    Len35([u8; 35]),
}

impl ScriptBytes {
    /// Returns the script bytes as a slice.
    pub fn as_slice(&self) -> &[u8] {
        match self {
            Self::Len34(b) => b,
            Self::Len35(b) => b,
        }
    }
}

impl AsRef<[u8]> for ScriptBytes {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}
