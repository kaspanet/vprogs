use risc0_binfmt::ProgramBinary;
use risc0_zkos_v1compat::V1COMPAT_ELF;

/// A guest ELF wrapped with the trusted v1compat kernel, paired with its image id.
pub struct ElfBinary {
    /// Guest image id.
    pub id: [u8; 32],
    /// v1compat-wrapped, encoded ELF bytes.
    pub elf: Vec<u8>,
}

impl ElfBinary {
    /// Wraps a raw guest ELF with the trusted v1compat kernel and computes its image id.
    pub(crate) fn new(raw_elf: &[u8]) -> Self {
        let binary = ProgramBinary::new(raw_elf, V1COMPAT_ELF);
        let digest = binary.compute_image_id().expect("image id");
        Self { id: digest.as_bytes().try_into().unwrap(), elf: binary.encode() }
    }
}
