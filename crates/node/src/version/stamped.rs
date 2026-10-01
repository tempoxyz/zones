//! Fixed-size ELF revision payload, populated after compilation and linking.

// The runtime reference keeps this allocated section alive through linker GC.
// Packaging changes its contents, never its size or address.
#[used]
#[unsafe(link_section = ".tempo_revision")]
static REVISION: [u8; 40] = [b'?'; 40];

pub(super) fn revision() -> String {
    // SAFETY: REVISION is an initialized, aligned static. It is modified only in
    // the executable file before execution, never concurrently in this process.
    // A volatile read prevents LLVM/LTO from substituting the placeholder value.
    let bytes = unsafe { std::ptr::read_volatile(&raw const REVISION) };
    assert!(
        bytes.iter().all(u8::is_ascii_hexdigit),
        "binary must be stamped with a full Git SHA before distribution"
    );
    String::from_utf8(bytes.to_vec()).expect("hexadecimal revision is valid UTF-8")
}
