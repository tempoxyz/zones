#!/usr/bin/env bash
# Stamp an unstamped Linux executable without compiling or linking it again.
set -euo pipefail

if [[ $# != 3 || ! "$3" =~ ^[0-9a-f]{40}$ ]]; then
    echo 'usage: stamp-revision.sh INPUT OUTPUT FULL_LOWERCASE_GIT_SHA' >&2
    exit 1
fi
input=$1
output=$2
revision=$3
if [[ "$input" == "$output" || "$input" -ef "$output" ]]; then
    echo 'input and output must be different files' >&2
    exit 1
fi

stamp_dir=$(mktemp -d)
trap 'rm -r -- "$stamp_dir"' EXIT
objcopy --dump-section .tempo_revision="$stamp_dir/original" "$input" "$stamp_dir/checked"
printf '%040d' 0 | tr '0' '?' > "$stamp_dir/placeholder"
if ! cmp -s "$stamp_dir/original" "$stamp_dir/placeholder"; then
    echo 'expected one unstamped 40-byte .tempo_revision section' >&2
    exit 1
fi
printf '%s' "$revision" > "$stamp_dir/revision"
objcopy --update-section .tempo_revision="$stamp_dir/revision" "$input" "$stamp_dir/stamped"
objcopy --dump-section .tempo_revision="$stamp_dir/actual" "$stamp_dir/stamped" "$stamp_dir/verified"
cmp "$stamp_dir/revision" "$stamp_dir/actual"
install -m 0755 "$stamp_dir/stamped" "$output"
