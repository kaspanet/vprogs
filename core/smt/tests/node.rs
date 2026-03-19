use vprogs_core_smt::{Blake3, Node};

#[test]
fn internal_roundtrip() {
    let node = Node::internal::<Blake3>(&[1u8; 32], &[2u8; 32]);
    let bytes = node.encode();
    assert_eq!(bytes.len(), 33);
    assert_eq!(Node::decode(&mut bytes.as_slice()).unwrap(), node);
}

#[test]
fn leaf_roundtrip() {
    let node = Node::leaf::<Blake3>([1u8; 32], [2u8; 32]);
    let bytes = node.encode();
    assert_eq!(bytes.len(), 97);
    assert_eq!(Node::decode(&mut bytes.as_slice()).unwrap(), node);
}
