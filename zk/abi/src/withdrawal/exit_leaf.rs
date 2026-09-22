//! Owned exit-leaf form for host-side channels. `StandardSpk<'a>` borrows its buffer, which
//! cannot cross an `await` point or a `watch` channel; this is the owned mirror. The canonical
//! leaf preimage stays `StandardSpk::to_script_bytes()`; `script_bytes` re-slices it verbatim.

use crate::withdrawal::standard_spk::StandardSpk;

/// Owned exit leaf pairing an on-chain destination script with a sompi payout amount.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitLeaf {
    /// On-chain script bytes buffer sized for the largest supported script.
    script_bytes: [u8; 35],
    /// Length of the active script bytes within `script_bytes`.
    script_len: u8,
    /// Exit payout amount in sompis.
    pub amount: u64,
}

impl ExitLeaf {
    /// Builds an owned leaf from a borrowed destination and payout amount.
    pub fn from_pair(dest: StandardSpk<'_>, amount: u64) -> Self {
        let bytes = dest.to_script_bytes();
        let slice = bytes.as_ref();
        let mut script_bytes = [0u8; 35];
        script_bytes[..slice.len()].copy_from_slice(slice);
        Self { script_bytes, script_len: slice.len() as u8, amount }
    }

    /// Returns the on-chain script bytes slice.
    pub fn script_bytes(&self) -> &[u8] {
        &self.script_bytes[..self.script_len as usize]
    }

    /// Reconstructs the borrowed destination view over this leaf's script bytes.
    pub fn to_standard_spk(&self) -> StandardSpk<'_> {
        StandardSpk::from_script(self.script_bytes()).expect("owned leaf decodes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_all_standard_spk_kinds() {
        let pk: &[u8; 32] = &[7u8; 32];
        let ecdsa: &[u8; 33] = &[9u8; 33];
        let sh: &[u8; 32] = &[3u8; 32];
        for (spk, amount) in [
            (StandardSpk::PubKey(pk), 1_000u64),
            (StandardSpk::PubKeyEcdsa(ecdsa), 2_000),
            (StandardSpk::ScriptHash(sh), 3_000),
        ] {
            let leaf = ExitLeaf::from_pair(spk, amount);
            assert_eq!(leaf.script_bytes(), spk.to_script_bytes().as_ref());
            assert_eq!(leaf.amount, amount);
            let back = leaf.to_standard_spk();
            assert_eq!(back, spk);
        }
    }
}
