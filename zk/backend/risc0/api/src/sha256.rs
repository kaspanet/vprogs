//! SHA-256 hasher for the api crate: the RISC-0 precompile under `guest`, software [`sha2`]
//! otherwise.
//!
//! Under `guest`, the risc0 rust-crypto wrapper computes the padding bit length in `u32`, so
//! any input of 2^32 bits (512 MiB) or more wraps the length field and produces a digest that
//! disagrees with the host-side software hasher. Inputs at or above that size are out of
//! contract for the guest hasher until the upstream arithmetic is fixed; debug builds reject
//! them at `GUEST_INPUT_CAP`.

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

    /// Upper bound on the bytes one guest SHA-256 may hash: 2^29 bytes = 2^32 bits, where the
    /// risc0 rust-crypto wrapper's `u32` bit-length arithmetic wraps.
    const GUEST_INPUT_CAP: u64 = 1 << 29;

    /// SHA-256 hasher dispatching to the RISC-0 SHA-256 precompile via [`risc0_zkvm::sha::Impl`].
    pub struct Sha256;

    /// Incremental SHA-256 state, wrapping the rust-crypto wrapper so streamed updates still
    /// dispatch to the precompile.
    #[derive(Default)]
    pub struct Sha256Incremental {
        /// The wrapped digest state.
        hasher: RustCryptoSha256,
        /// Bytes fed so far, enforcing [`GUEST_INPUT_CAP`] in debug builds.
        fed: u64,
    }

    impl IncrementalHasher for Sha256Incremental {
        fn update(&mut self, data: &[u8]) {
            self.fed = self.fed.saturating_add(data.len() as u64);
            debug_assert!(
                self.fed < GUEST_INPUT_CAP,
                "guest sha256 input reached the risc0 u32 bit-length wrap at 512 MiB"
            );
            Digest::update(&mut self.hasher, data);
        }

        fn finalize(self) -> [u8; 32] {
            Digest::finalize(self.hasher).into()
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
