#!/usr/bin/env bash
set -euo pipefail
root="${1:?usage: prepare-tempo-xtask.sh TEMPO_ROOT}"
bench_dir="$(cd "$(dirname "$0")" && pwd)"
# New Tempo revisions include file-backed secrets natively and moved the bloat
# generator into its own crate. Older pinned revisions still need the patch.
bloat_source="$root/crates/state-bloat/src/generate.rs"
if [[ ! -f "$bloat_source" ]]; then
    bloat_source="$root/xtask/src/generate_state_bloat.rs"
fi
if grep -q 'mnemonic_file: Option<PathBuf>' "$root/xtask/src/genesis_args.rs" \
    && grep -q 'mnemonic_file: Option<PathBuf>' "$bloat_source"; then
    echo 'Pinned Tempo already supports file-backed mnemonics'
else
    git -C "$root" apply --check "$bench_dir/patches/tempo-xtask-mnemonic-file.patch"
    git -C "$root" apply "$bench_dir/patches/tempo-xtask-mnemonic-file.patch"
fi
