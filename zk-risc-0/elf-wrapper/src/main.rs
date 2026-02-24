/// Wraps a raw RISC-V ELF into the ProgramBinary format expected by the RISC-0 executor.
///
/// Usage: elf-wrapper <path>
///
/// Reads the raw ELF at <path>, combines it with the v1compat kernel, and overwrites the file
/// with the wrapped ProgramBinary encoding.
fn main() {
    let path = std::env::args().nth(1).expect("usage: elf-wrapper <elf-path>");
    let user_elf = std::fs::read(&path).expect("failed to read ELF");
    let wrapped = risc0_binfmt::ProgramBinary::new(&user_elf, risc0_zkos_v1compat::V1COMPAT_ELF);
    std::fs::write(&path, wrapped.encode()).expect("failed to write wrapped ELF");
}
