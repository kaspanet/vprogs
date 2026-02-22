//! Generic sub-proof framework — `no_std`, usable inside any RISC-0 guest.
//!
//! Handles all boilerplate: witness parsing, hashing, verification, effects tree
//! construction, and journal formatting. The user only provides an execution
//! function via a closure.
//!
//! ## Guest usage
//!
//! ```ignore
//! let raw = read_stdin_bytes();
//! let input = SubProofInput::from_bytes(&raw).unwrap();
//! let output = run_sub_proof(&raw, input, |tx_data, resources| {
//!     my_l2_logic(tx_data, resources)
//! });
//! commit(output.journal.to_bytes());   // → journal (proven, 68 bytes)
//! write(output.encode_post_states());  // → stdout  (unproven, for host)
//! ```
//!
//! ## Wire format (stdin, written by host)
//!
//! ```text
//! tx_index       : u32 (LE)
//! num_resources  : u32 (LE)
//! For each resource:
//!   resource_id_hash : [u8; 32]
//!   access_type      : u8        (0 = Read, 1 = Write)
//!   state_len        : u32 (LE)
//!   pre_state        : [u8; state_len]
//! tx_data_len    : u32 (LE)
//! tx_data        : [u8; tx_data_len]
//! ```

use alloc::vec::Vec;

use crate::{
    effects::{AccessEffect, effects_root},
    hashing::state_hash,
    journal::{SubProofJournal, context_hash},
};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// One resource's input to the sub-proof guest.
#[derive(Clone, Debug)]
pub struct ResourceInput {
    pub resource_id_hash: [u8; 32],
    /// 0 = Read, 1 = Write.
    pub access_type: u8,
    /// Serialized pre-state data.
    pub pre_state: Vec<u8>,
}

/// Full parsed witness for a single sub-proof.
#[derive(Clone, Debug)]
pub struct SubProofInput {
    pub tx_index: u32,
    pub resources: Vec<ResourceInput>,
    /// Opaque transaction payload.
    pub tx_data: Vec<u8>,
}

/// Output produced by [`run_sub_proof`].
pub struct SubProofOutput {
    /// Succinct journal to commit (68 bytes, proven).
    pub journal: SubProofJournal,
    /// Per-resource post-state data (same order as input resources).
    /// For reads this equals the pre-state.
    /// Write to stdout (unproven) so the host can persist.
    pub post_states: Vec<Vec<u8>>,
}

// ---------------------------------------------------------------------------
// Framework entry-point
// ---------------------------------------------------------------------------

/// Run the generic sub-proof framework.
///
/// 1. Computes `context_hash` over `raw_witness` (the raw stdin bytes).
/// 2. Calls `execute(tx_data, resources)` → post-states.
/// 3. Verifies read consistency (post == pre for reads).
/// 4. Builds per-resource effects and merkle root.
/// 5. Returns the succinct journal + post-states.
///
/// The caller is responsible for writing `output.journal` to the RISC-0
/// journal and `output.encode_post_states()` to stdout.
pub fn run_sub_proof(
    raw_witness: &[u8],
    input: SubProofInput,
    execute: impl FnOnce(&[u8], &[ResourceInput]) -> Vec<Vec<u8>>,
) -> SubProofOutput {
    let ctx_hash = context_hash(raw_witness);

    // Execute the pluggable L2 logic.
    let post_states = execute(&input.tx_data, &input.resources);
    assert_eq!(
        post_states.len(),
        input.resources.len(),
        "execute must return one post_state per resource"
    );

    // Build effects from pre/post hashes.
    let mut effects = Vec::with_capacity(input.resources.len());
    for (resource, post_state) in input.resources.iter().zip(post_states.iter()) {
        let pre_hash = state_hash(&resource.pre_state);
        let post_hash = state_hash(post_state);

        if resource.access_type == 0 {
            assert_eq!(pre_hash, post_hash, "read access must not change state");
        }

        effects.push(AccessEffect {
            resource_id_hash: resource.resource_id_hash,
            access_type: resource.access_type,
            pre_hash,
            post_hash,
        });
    }

    let root = effects_root(&effects);

    SubProofOutput {
        journal: SubProofJournal {
            tx_index: input.tx_index,
            effects_root: root,
            context_hash: ctx_hash,
        },
        post_states,
    }
}

// ---------------------------------------------------------------------------
// Serialization
// ---------------------------------------------------------------------------

impl SubProofInput {
    /// Deserialize from the wire format described in the module docs.
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        let mut pos = 0;

        let tx_index = read_u32(data, &mut pos)?;
        let num_resources = read_u32(data, &mut pos)? as usize;

        let mut resources = Vec::with_capacity(num_resources);
        for _ in 0..num_resources {
            if pos + 32 > data.len() {
                return None;
            }
            let mut resource_id_hash = [0u8; 32];
            resource_id_hash.copy_from_slice(&data[pos..pos + 32]);
            pos += 32;

            if pos >= data.len() {
                return None;
            }
            let access_type = data[pos];
            pos += 1;

            let state_len = read_u32(data, &mut pos)? as usize;
            if pos + state_len > data.len() {
                return None;
            }
            let pre_state = data[pos..pos + state_len].to_vec();
            pos += state_len;

            resources.push(ResourceInput { resource_id_hash, access_type, pre_state });
        }

        let tx_data_len = read_u32(data, &mut pos)? as usize;
        if pos + tx_data_len > data.len() {
            return None;
        }
        let tx_data = data[pos..pos + tx_data_len].to_vec();
        pos += tx_data_len;

        if pos != data.len() {
            return None;
        }

        Some(Self { tx_index, resources, tx_data })
    }

    /// Serialize to the wire format.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&self.tx_index.to_le_bytes());
        buf.extend_from_slice(&(self.resources.len() as u32).to_le_bytes());

        for r in &self.resources {
            buf.extend_from_slice(&r.resource_id_hash);
            buf.push(r.access_type);
            buf.extend_from_slice(&(r.pre_state.len() as u32).to_le_bytes());
            buf.extend_from_slice(&r.pre_state);
        }

        buf.extend_from_slice(&(self.tx_data.len() as u32).to_le_bytes());
        buf.extend_from_slice(&self.tx_data);
        buf
    }
}

impl SubProofOutput {
    /// Encode post-states for writing to stdout (unproven channel).
    ///
    /// Layout: for each resource, `state_len(u32 LE) || state_data`.
    pub fn encode_post_states(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        for ps in &self.post_states {
            buf.extend_from_slice(&(ps.len() as u32).to_le_bytes());
            buf.extend_from_slice(ps);
        }
        buf
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn read_u32(data: &[u8], pos: &mut usize) -> Option<u32> {
    if *pos + 4 > data.len() {
        return None;
    }
    let val = u32::from_le_bytes(data[*pos..*pos + 4].try_into().unwrap());
    *pos += 4;
    Some(val)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn input_roundtrip() {
        let input = SubProofInput {
            tx_index: 42,
            resources: vec![
                ResourceInput {
                    resource_id_hash: [1u8; 32],
                    access_type: 1,
                    pre_state: vec![10, 20, 30],
                },
                ResourceInput {
                    resource_id_hash: [2u8; 32],
                    access_type: 0,
                    pre_state: vec![40, 50],
                },
            ],
            tx_data: vec![0xAA, 0xBB, 0xCC],
        };

        let bytes = input.to_bytes();
        let parsed = SubProofInput::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.tx_index, 42);
        assert_eq!(parsed.resources.len(), 2);
        assert_eq!(parsed.resources[0].resource_id_hash, [1u8; 32]);
        assert_eq!(parsed.resources[0].access_type, 1);
        assert_eq!(parsed.resources[0].pre_state, vec![10, 20, 30]);
        assert_eq!(parsed.resources[1].access_type, 0);
        assert_eq!(parsed.resources[1].pre_state, vec![40, 50]);
        assert_eq!(parsed.tx_data, vec![0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn input_empty_resources() {
        let input = SubProofInput { tx_index: 0, resources: vec![], tx_data: vec![0xFF] };
        let bytes = input.to_bytes();
        let parsed = SubProofInput::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.resources.len(), 0);
        assert_eq!(parsed.tx_data, vec![0xFF]);
    }

    #[test]
    fn input_trailing_bytes_rejected() {
        let input = SubProofInput { tx_index: 0, resources: vec![], tx_data: vec![] };
        let mut bytes = input.to_bytes();
        bytes.push(0xFF); // extra byte
        assert!(SubProofInput::from_bytes(&bytes).is_none());
    }

    #[test]
    fn run_write() {
        let input = SubProofInput {
            tx_index: 0,
            resources: vec![ResourceInput {
                resource_id_hash: *blake3::hash(b"r1").as_bytes(),
                access_type: 1,
                pre_state: vec![1, 2, 3],
            }],
            tx_data: vec![0xFF],
        };
        let raw = input.to_bytes();

        let output = run_sub_proof(&raw, input, |_tx_data, _resources| {
            vec![vec![4, 5, 6]] // new state
        });

        assert_eq!(output.journal.tx_index, 0);
        assert_ne!(output.journal.effects_root, [0u8; 32]);
        assert_ne!(output.journal.context_hash, [0u8; 32]);
        assert_eq!(output.post_states, vec![vec![4, 5, 6]]);
    }

    #[test]
    fn run_read() {
        let input = SubProofInput {
            tx_index: 1,
            resources: vec![ResourceInput {
                resource_id_hash: *blake3::hash(b"r1").as_bytes(),
                access_type: 0,
                pre_state: vec![1, 2, 3],
            }],
            tx_data: vec![],
        };
        let raw = input.to_bytes();

        let output =
            run_sub_proof(&raw, input, |_tx_data, resources| vec![resources[0].pre_state.clone()]);

        assert_eq!(output.post_states, vec![vec![1, 2, 3]]);
    }

    #[test]
    #[should_panic(expected = "read access must not change state")]
    fn run_read_changed_panics() {
        let input = SubProofInput {
            tx_index: 0,
            resources: vec![ResourceInput {
                resource_id_hash: *blake3::hash(b"r1").as_bytes(),
                access_type: 0,
                pre_state: vec![1, 2, 3],
            }],
            tx_data: vec![],
        };
        let raw = input.to_bytes();

        run_sub_proof(&raw, input, |_, _| vec![vec![4, 5, 6]]);
    }

    #[test]
    #[should_panic(expected = "execute must return one post_state per resource")]
    fn run_wrong_count_panics() {
        let input = SubProofInput {
            tx_index: 0,
            resources: vec![ResourceInput {
                resource_id_hash: [0u8; 32],
                access_type: 1,
                pre_state: vec![1],
            }],
            tx_data: vec![],
        };
        let raw = input.to_bytes();

        run_sub_proof(&raw, input, |_, _| vec![]); // 0 results for 1 resource
    }

    #[test]
    fn context_hash_binds_to_input() {
        let input_a = SubProofInput {
            tx_index: 0,
            resources: vec![ResourceInput {
                resource_id_hash: *blake3::hash(b"r1").as_bytes(),
                access_type: 1,
                pre_state: vec![1],
            }],
            tx_data: vec![0xAA],
        };
        let input_b = SubProofInput {
            tx_index: 0,
            resources: vec![ResourceInput {
                resource_id_hash: *blake3::hash(b"r1").as_bytes(),
                access_type: 1,
                pre_state: vec![1],
            }],
            tx_data: vec![0xBB], // different tx_data
        };

        let raw_a = input_a.to_bytes();
        let raw_b = input_b.to_bytes();

        let out_a = run_sub_proof(&raw_a, input_a, |_, _| vec![vec![2]]);
        let out_b = run_sub_proof(&raw_b, input_b, |_, _| vec![vec![2]]);

        // Same post-state, same effects_root — but different context_hash.
        assert_eq!(out_a.journal.effects_root, out_b.journal.effects_root);
        assert_ne!(out_a.journal.context_hash, out_b.journal.context_hash);
    }

    #[test]
    fn encode_post_states_roundtrip() {
        let output = SubProofOutput {
            journal: SubProofJournal {
                tx_index: 0,
                effects_root: [0u8; 32],
                context_hash: [0u8; 32],
            },
            post_states: vec![vec![1, 2, 3], vec![4, 5]],
        };
        let encoded = output.encode_post_states();

        // Parse back manually.
        let mut pos = 0;
        let len0 = u32::from_le_bytes(encoded[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        assert_eq!(&encoded[pos..pos + len0], &[1, 2, 3]);
        pos += len0;
        let len1 = u32::from_le_bytes(encoded[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        assert_eq!(&encoded[pos..pos + len1], &[4, 5]);
    }

    #[test]
    fn mixed_read_write() {
        let input = SubProofInput {
            tx_index: 5,
            resources: vec![
                ResourceInput {
                    resource_id_hash: *blake3::hash(b"r1").as_bytes(),
                    access_type: 0, // read
                    pre_state: vec![10, 20],
                },
                ResourceInput {
                    resource_id_hash: *blake3::hash(b"r2").as_bytes(),
                    access_type: 1, // write
                    pre_state: vec![30, 40],
                },
            ],
            tx_data: vec![0xDD],
        };
        let raw = input.to_bytes();

        let output = run_sub_proof(&raw, input, |_tx_data, resources| {
            vec![
                resources[0].pre_state.clone(), // read → unchanged
                vec![50, 60],                   // write → new state
            ]
        });

        assert_eq!(output.journal.tx_index, 5);
        assert_eq!(output.post_states[0], vec![10, 20]);
        assert_eq!(output.post_states[1], vec![50, 60]);
    }
}
