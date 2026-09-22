//! Host-side exit-leaf feed. The guest accumulator folds exits inside the aggregator proof;
//! the host replays the same leaves from batch journals so clients can build merkle paths.

use std::sync::Arc;

use vprogs_zk_abi::{Error, batch_processor::BatchTransition, withdrawal::ExitLeaf};
use zerocopy::FromBytes;

/// Bundle exits extracted from batch journals alongside the state root and permission hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitsForBundle {
    /// L2 SMT state root after this bundle.
    pub new_state: [u8; 32],
    /// Hash of the permission redeem script, or `[0u8; 32]` if no exits were emitted.
    pub permission_spk_hash: [u8; 32],
    /// Owned exit leaves extracted in batch and journal order. The bundle's complete canonical
    /// leaf list for that root (per-bundle tree; never empty when published: the worker suppresses
    /// zero-hash bundles).
    pub leaves: Arc<Vec<ExitLeaf>>,
}

/// Decodes trailing exits from a sequence of batch journals in bundle order.
pub fn extract_bundle_exits(journals: &[Vec<u8>]) -> Result<Vec<ExitLeaf>, Error> {
    let mut leaves = Vec::new();
    for journal in journals {
        let transition = BatchTransition::ref_from_bytes(journal.as_slice())
            .map_err(|e| Error::Decode(format!("{e}")))?;
        for pair in &transition.exits {
            let (dest, amount) = pair?;
            leaves.push(ExitLeaf::from_pair(dest, amount));
        }
    }
    Ok(leaves)
}

#[cfg(test)]
mod tests {
    use kaspa_hashes::Hash;
    use vprogs_zk_abi::{
        batch_processor::{BatchTransition, BatchTransitionArgs},
        withdrawal::StandardSpk,
    };

    use super::*;

    fn journal_with_exits(pairs: &[(Vec<u8>, u64)]) -> Vec<u8> {
        let mut exits_buf = Vec::new();
        for (script_bytes, amount) in pairs {
            let spk = StandardSpk::from_script(script_bytes).expect("valid script");
            spk.encode(&mut exits_buf);
            exits_buf.extend_from_slice(&amount.to_le_bytes());
        }
        let mut buf = Vec::new();
        BatchTransition::encode(
            &mut buf,
            BatchTransitionArgs {
                prev_state: &[0u8; 32],
                prev_lane_tip: &Hash::default(),
                prev_lane_blue_score: 0,
                new_state: &[0u8; 32],
                new_lane_tip: &Hash::default(),
                new_lane_blue_score: 0,
                lane_key: &Hash::default(),
                covenant_id: &[0u8; 32],
                tx_image_id: &[0u8; 32],
                deposit_spk_hash: &[0u8; 32],
                lane_expired: false,
                exits: &exits_buf,
            },
        );
        buf
    }

    #[test]
    fn extracts_leaves_in_journal_order() {
        let a: Vec<u8> = StandardSpk::PubKey(&[1u8; 32]).to_script_bytes().as_ref().to_vec();
        let b: Vec<u8> = StandardSpk::ScriptHash(&[2u8; 32]).to_script_bytes().as_ref().to_vec();
        let j1 = journal_with_exits(&[(a.clone(), 100), (b.clone(), 200)]);
        let j2 = journal_with_exits(&[(a.clone(), 300)]);
        let leaves = extract_bundle_exits(&[j1, j2]).unwrap();
        assert_eq!(leaves.len(), 3);
        assert_eq!(leaves[0].amount, 100);
        assert_eq!(leaves[0].script_bytes(), a.as_slice());
        assert_eq!(leaves[1].amount, 200);
        assert_eq!(leaves[2].amount, 300);
    }

    #[test]
    fn handles_empty_bundle_and_empty_exits() {
        let empty_bundle = extract_bundle_exits(&[]).unwrap();
        assert!(empty_bundle.is_empty());

        let j_no_exits = journal_with_exits(&[]);
        let empty_leaves = extract_bundle_exits(&[j_no_exits.clone(), j_no_exits]).unwrap();
        assert!(empty_leaves.is_empty());
    }

    #[test]
    fn rejects_truncated_journal() {
        let bad = vec![0u8; 10];
        assert!(extract_bundle_exits(&[bad]).is_err());
    }
}
