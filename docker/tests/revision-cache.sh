#!/usr/bin/env bash
# Full real-node comparison, run on a Docker-capable runner with Depot auth.
set -euo pipefail
cd "$(dirname "$0")/../.."
evidence="$RUNNER_TEMP/revision-cache-evidence"
mkdir -p "$evidence"
sha_a=$(git rev-parse HEAD)
tree=$(git rev-parse 'HEAD^{tree}')
sha_b=$(printf 'Revision-only benchmark commit\n' | \
    git -c user.name='Revision cache test' -c user.email='test@example.invalid' \
    commit-tree "$tree" -p "$sha_a")
[[ $(git rev-parse "$sha_b^{tree}") == "$tree" ]]
printf 'commit_a=%s\ncommit_b=%s\ntree=%s\n' "$sha_a" "$sha_b" "$tree" > "$evidence/inputs.txt"

# The historical Dockerfile is the only difference between the two experiments.
# Both use today's exact same source tree, flags and dependency recipe. It reads
# the revision at compile time; the new Dockerfile reads it only while packaging.
git show 344ff78573e6cfc35c8c551b4dc39b042121fc2b:docker/Dockerfile > "$RUNNER_TEMP/revision-baseline.Dockerfile"
printf 'variant\tseconds\n' > "$evidence/timings.tsv"

build() {
    local name=$1 sha=$2 dockerfile=$3 start
    start=$(date +%s)
    VERGEN_GIT_SHA="$sha" depot bake --project 0c6tg19qsp \
        --file docker/docker-bake.hcl --progress plain --load \
        --set "tempo-zone.dockerfile=$dockerfile" \
        --set "tempo-zone.tags=revision-cache:$name" tempo-zone \
        2>&1 | tee "$evidence/$name.log"
    printf '%s\t%s\n' "$name" "$(( $(date +%s) - start ))" >> "$evidence/timings.tsv"
}

build baseline-a "$sha_a" "$RUNNER_TEMP/revision-baseline.Dockerfile"
build baseline-b "$sha_b" "$RUNNER_TEMP/revision-baseline.Dockerfile"
build stamped-a "$sha_a" docker/Dockerfile
build stamped-b "$sha_b" docker/Dockerfile

# Match the actual compilation vertex, not cached apt/copy/dependency vertices.
awk '
    /\[.* builder .*\] RUN .*cargo build/ { vertex = $1 }
    vertex && $1 == vertex && $2 == "CACHED" { hit = 1 }
    END { exit !hit }
' "$evidence/stamped-b.log"

for variant in baseline-a baseline-b stamped-a stamped-b; do
    container=$(docker create "revision-cache:$variant")
    docker cp "$container:/usr/local/bin/tempo-zone" "$RUNNER_TEMP/$variant"
    docker rm "$container"
    # Intentionally execute outside Docker: no revision file or runtime env.
    "$RUNNER_TEMP/$variant" --version | tee "$evidence/$variant.version.txt"
    expected=$sha_a
    [[ "$variant" == *-a ]] || expected=$sha_b
    grep -F "Commit SHA: $expected" "$evidence/$variant.version.txt"
    if [[ "$variant" == stamped-* ]]; then
        [[ $(docker image inspect "revision-cache:$variant" --format '{{index .Config.Labels "org.opencontainers.image.revision"}}') == "$expected" ]]
    fi
done

objcopy --dump-section .text="$RUNNER_TEMP/text-a" "$RUNNER_TEMP/stamped-a" "$RUNNER_TEMP/checked"
objcopy --dump-section .text="$RUNNER_TEMP/text-b" "$RUNNER_TEMP/stamped-b" "$RUNNER_TEMP/checked"
cmp "$RUNNER_TEMP/text-a" "$RUNNER_TEMP/text-b"
printf '%s' "$sha_a" > "$RUNNER_TEMP/revision-payload"
objcopy --update-section .tempo_revision="$RUNNER_TEMP/revision-payload" "$RUNNER_TEMP/stamped-b" "$RUNNER_TEMP/normalized-b"
cmp "$RUNNER_TEMP/stamped-a" "$RUNNER_TEMP/normalized-b"
sha256sum "$RUNNER_TEMP/stamped-a" "$RUNNER_TEMP/stamped-b" "$RUNNER_TEMP/text-a" > "$evidence/checksums.txt"
strip --strip-all "$RUNNER_TEMP/stamped-b"
"$RUNNER_TEMP/stamped-b" --version | grep -F "Commit SHA: $sha_b"
{
    echo '## SHA-only rebuild comparison'
    echo 'Warm rebuilds are baseline-b and stamped-b; the -a runs warm their respective compilation layers.'
    echo '```'
    cat "$evidence/inputs.txt" "$evidence/timings.tsv"
    echo '```'
    echo 'PASS: stamped-b compilation vertex CACHED; extracted binaries report the correct SHA; code identical; only the revision payload differs; stripping preserves the stamp.'
} | tee "$evidence/summary.md" >> "$GITHUB_STEP_SUMMARY"
