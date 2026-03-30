use sp1_build::{BuildArgs, build_program_with_args};

fn main() {
    // SP1 guest builds for the custom `riscv{32,64}im-succinct-zkvm-elf` targets can
    // pull native deps (e.g. secp256k1-sys, blst) through the Tempo/revm stack.
    // `cc` does not understand the target flags; default to clang/llvm-ar unless
    // the caller already provided explicit target tool overrides.
    for (cc_key, ar_key, cflags_key, cflags_value) in [
        (
            "CC_riscv32im_succinct_zkvm_elf",
            "AR_riscv32im_succinct_zkvm_elf",
            "CFLAGS_riscv32im_succinct_zkvm_elf",
            "-march=rv32im -mabi=ilp32 -mno-relax",
        ),
        (
            "CC_riscv64im_succinct_zkvm_elf",
            "AR_riscv64im_succinct_zkvm_elf",
            "CFLAGS_riscv64im_succinct_zkvm_elf",
            "-march=rv64im -mabi=lp64 -mno-relax",
        ),
    ] {
        if std::env::var_os(cc_key).is_none() {
            unsafe { std::env::set_var(cc_key, "clang") };
            println!("cargo:warning=defaulting {cc_key}=clang");
        }
        if std::env::var_os(ar_key).is_none() {
            unsafe { std::env::set_var(ar_key, "llvm-ar") };
            println!("cargo:warning=defaulting {ar_key}=llvm-ar");
        }
        if std::env::var_os(cflags_key).is_none() {
            unsafe { std::env::set_var(cflags_key, cflags_value) };
            println!("cargo:warning=defaulting {cflags_key}={cflags_value}");
        }
    }

    let args = BuildArgs {
        output_directory: Some("elf".into()),
        elf_name: Some("zone-prover-sp1".into()),
        // The SP1 5.2.3+/LLVM21 toolchain currently emits RVC in guest ELFs, which
        // SP1 5.2.x local setup cannot disassemble. SP1 5.2.2 uses an older toolchain
        // without this issue, but reports an older rustc version than this workspace's
        // dependencies declare. Allow the guest build to proceed and rely on actual
        // compilation success/failure rather than `rust-version` metadata.
        ignore_rust_version: true,
        // SP1 SDK 5.2.x host-side disassembly/parsing path rejects compressed
        // RISC-V instructions for this guest. Force RV32IM (no `C`) so the
        // embedded ELF is accepted by `NetworkProver::setup`.
        rustflags: vec![
            "-C".into(),
            "target-feature=-c,-zca,-zcb,-zcd,-zcf,-zcmp,-zcmt".into(),
            // RISC-V link-time relaxation can rewrite RV32IM instructions into
            // compressed encodings and set the ELF RVC flag even when the target
            // spec disables `c`. SP1 5.2.x's local setup/disassembler path expects
            // plain 32-bit RV32IM words, so disable relaxation.
            "-C".into(),
            "link-arg=--no-relax".into(),
        ],
        ..Default::default()
    };

    build_program_with_args("program", args);
}
