use vprogs_zk_smt_v2::NodeData;

#[test]
fn internal_hash_accessor() {
    let h = [42u8; 32];
    let node = NodeData::Internal { hash: h };
    assert_eq!(node.hash(), &h);
}

#[test]
fn leaf_hash_accessor() {
    let h = [99u8; 32];
    let node = NodeData::Leaf { key: [1u8; 32], value_hash: [2u8; 32], hash: h };
    assert_eq!(node.hash(), &h);
}

#[test]
fn roundtrip_internal() {
    let node = NodeData::Internal { hash: [7u8; 32] };
    let bytes = node.to_bytes();
    assert_eq!(bytes.len(), 33);
    assert_eq!(NodeData::from_bytes(&bytes), node);
}

#[test]
fn roundtrip_leaf() {
    let node = NodeData::Leaf { key: [1u8; 32], value_hash: [2u8; 32], hash: [3u8; 32] };
    let bytes = node.to_bytes();
    assert_eq!(bytes.len(), 97);
    assert_eq!(NodeData::from_bytes(&bytes), node);
}
