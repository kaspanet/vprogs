//! Genesis key derivation and P2PK address helpers.
//!
//! Provides [`dev_genesis_keypair`] for the default dev genesis keypair (secp256k1 scalar 3)
//! and [`p2pk_address`] for deriving network-prefixed P2PK addresses.

use kaspa_addresses::{Address, Prefix, Version};
use kaspa_consensus_core::config::params::Params;
use secp256k1::{Keypair, SECP256K1, SecretKey};

/// Creates the development genesis keypair (secp256k1 scalar `3`, BIP-340 test vector 0).
///
/// Panics if the generated x-only public key does not match `expected_pubkey` (such as the app's
/// compiled `GENESIS_PUBKEY` constant), alerting operators immediately when running against
/// custom-genesis guests without providing the matching private key.
pub fn dev_genesis_keypair(expected_pubkey: &[u8; 32]) -> Keypair {
    let mut secret = [0u8; 32];
    secret[31] = 3;
    let sk = SecretKey::from_slice(&secret).expect("scalar 3 is a valid secp256k1 secret key");
    let keypair = Keypair::from_secret_key(SECP256K1, &sk);
    let actual_pubkey = keypair.x_only_public_key().0.serialize();
    assert_eq!(
        &actual_pubkey, expected_pubkey,
        "dev genesis keypair mismatch: expected {:?}, got {:?}",
        expected_pubkey, actual_pubkey,
    );
    keypair
}

/// Derives the P2PK address for `pubkey` under the network prefix configured in `params`.
pub fn p2pk_address(params: &Params, pubkey: &[u8; 32]) -> Address {
    let prefix = Prefix::from(params.net.network_type());
    Address::new(prefix, Version::PubKey, pubkey)
}

#[cfg(test)]
mod tests {
    use kaspa_consensus_core::network::{NetworkId, NetworkType};
    use vprogs_zk_backend_risc0_runtime_processor::genesis::GENESIS_SCHNORR_BYTES;

    use super::*;

    #[test]
    fn test_dev_genesis_keypair_matches_bip340_vector_0() {
        let keypair = dev_genesis_keypair(&GENESIS_SCHNORR_BYTES);
        assert_eq!(keypair.x_only_public_key().0.serialize(), GENESIS_SCHNORR_BYTES);
    }

    #[test]
    #[should_panic(expected = "dev genesis keypair mismatch")]
    fn test_dev_genesis_keypair_mismatch_panics() {
        let wrong_pubkey = [0x00u8; 32];
        dev_genesis_keypair(&wrong_pubkey);
    }

    #[test]
    fn test_p2pk_address_network_prefixes() {
        let pubkey = [0x42u8; 32];

        let mainnet_params = Params::from(NetworkId::new(NetworkType::Mainnet));
        let addr_mainnet = p2pk_address(&mainnet_params, &pubkey);
        assert_eq!(addr_mainnet.prefix, Prefix::Mainnet);
        assert_eq!(&addr_mainnet.payload[..], &pubkey[..]);

        let testnet_params = Params::from(NetworkId::with_suffix(NetworkType::Testnet, 10));
        let addr_testnet = p2pk_address(&testnet_params, &pubkey);
        assert_eq!(addr_testnet.prefix, Prefix::Testnet);
        assert_eq!(&addr_testnet.payload[..], &pubkey[..]);

        let simnet_params = Params::from(NetworkId::new(NetworkType::Simnet));
        let addr_simnet = p2pk_address(&simnet_params, &pubkey);
        assert_eq!(addr_simnet.prefix, Prefix::Simnet);
        assert_eq!(&addr_simnet.payload[..], &pubkey[..]);

        let devnet_params = Params::from(NetworkId::new(NetworkType::Devnet));
        let addr_devnet = p2pk_address(&devnet_params, &pubkey);
        assert_eq!(addr_devnet.prefix, Prefix::Devnet);
        assert_eq!(&addr_devnet.payload[..], &pubkey[..]);
    }
}
