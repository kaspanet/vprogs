//! SHA-256 hasher for the api crate: the RISC-0 precompile under `guest`, software [`sha2`]
//! otherwise.

#[cfg(feature = "guest")]
pub use precompile::Sha256;
#[cfg(not(feature = "guest"))]
pub use vprogs_core_hashing::Sha256;

#[cfg(feature = "guest")]
mod precompile {
    use risc0_zkvm::sha::{
        Impl, Sha256 as RiscSha256Trait,
        rust_crypto::{Digest, Sha256 as RustCryptoSha256},
    };
    use vprogs_core_hashing::{Hasher, IncrementalHasher};

    /// SHA-256 hasher dispatching to the RISC-0 SHA-256 precompile via [`risc0_zkvm::sha::Impl`].
    pub struct Sha256;

    /// Incremental SHA-256 state, wrapping the rust-crypto wrapper so streamed updates still
    /// dispatch to the precompile.
    #[derive(Default)]
    pub struct Sha256Incremental(RustCryptoSha256);

    impl IncrementalHasher for Sha256Incremental {
        fn update(&mut self, data: &[u8]) {
            Digest::update(&mut self.0, data);
        }

        fn finalize(self) -> [u8; 32] {
            Digest::finalize(self.0).into()
        }
    }

    impl Hasher for Sha256 {
        type Incremental = Sha256Incremental;

        fn hash(data: impl AsRef<[u8]>) -> [u8; 32] {
            (*<Impl as RiscSha256Trait>::hash_bytes(data.as_ref())).into()
        }

        /// Prepends the domain bytes and streams the parts through the rust-crypto wrapper, which
        /// dispatches each block to the precompile -- no intermediate buffer.
        fn hash_parts_with_domain<const N: usize>(
            domain: &[u8; N],
            parts: impl IntoIterator<Item = impl AsRef<[u8]>>,
        ) -> [u8; 32] {
            let mut hasher = RustCryptoSha256::new();
            hasher.update(domain);
            for part in parts {
                hasher.update(part.as_ref());
            }
            hasher.finalize().into()
        }
    }
}
