#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
test_dir=$(mktemp -d)
trap 'rm -r -- "$test_dir"' EXIT
sha_a=1111111111111111111111111111111111111111
sha_b=2222222222222222222222222222222222222222

must_fail() {
    if "$@" > "$test_dir/error.log" 2>&1; then
        echo "unexpected success: $*" >&2
        exit 1
    fi
}

for lto in off thin fat; do
    rustc --edition=2024 -O -C "lto=$lto" docker/tests/revision.rs -o "$test_dir/raw"
    cp "$test_dir/raw" "$test_dir/original"
    must_fail "$test_dir/raw"
    for variant in unstripped stripped; do
        if [[ "$variant" == stripped ]]; then
            strip --strip-all "$test_dir/raw"
        fi
        bash docker/stamp-revision.sh "$test_dir/raw" "$test_dir/a" "$sha_a"
        bash docker/stamp-revision.sh "$test_dir/raw" "$test_dir/b" "$sha_b"
        [[ $("$test_dir/a") == "$sha_a" ]]
        [[ $("$test_dir/b") == "$sha_b" ]]
        # objcopy may normalize ELF layout. The two packaged files must differ
        # only in the revision, and executable code must match the input.
        printf '%040d' 0 | tr '0' '?' > "$test_dir/placeholder"
        objcopy --update-section .tempo_revision="$test_dir/placeholder" "$test_dir/a" "$test_dir/normalized"
        objcopy --update-section .tempo_revision="$test_dir/placeholder" "$test_dir/b" "$test_dir/normalized-b"
        cmp "$test_dir/normalized" "$test_dir/normalized-b"
        objcopy --dump-section .text="$test_dir/text-raw" "$test_dir/raw" "$test_dir/checked"
        objcopy --dump-section .text="$test_dir/text-stamped" "$test_dir/a" "$test_dir/checked"
        cmp "$test_dir/text-raw" "$test_dir/text-stamped"
        strip --strip-all "$test_dir/a"
        [[ $("$test_dir/a") == "$sha_a" ]]
        must_fail bash docker/stamp-revision.sh "$test_dir/a" "$test_dir/repeated" "$sha_b"
        echo "PASS: lto=$lto, input=$variant, exact-byte roundtrip and standalone revision"
    done
done
must_fail bash docker/stamp-revision.sh "$test_dir/raw" "$test_dir/raw" "$sha_a"
for sha in '' 1234567 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa gggggggggggggggggggggggggggggggggggggggg; do
    must_fail bash docker/stamp-revision.sh "$test_dir/raw" "$test_dir/invalid" "$sha"
done
objcopy --remove-section .tempo_revision "$test_dir/raw" "$test_dir/no-section"
must_fail bash docker/stamp-revision.sh "$test_dir/no-section" "$test_dir/invalid" "$sha_a"
echo 'PASS: invalid revisions, missing sections, in-place writes and restamping rejected'
