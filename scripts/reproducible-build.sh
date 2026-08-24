#!/usr/bin/env bash
# Reproducible-build wrapper. Single source of truth for producing the
# byte-deterministic `tempo-zone` binary for x86_64-unknown-linux-gnu.
#
# Inputs (env):
#   VERSION         informational version baked into audit output (default: dev)
#   OUT_DIR         where the built binary lands (default: ./out)
#   GIT_SHA         full SHA of the checked-out commit (validated when set)
#   SOURCE_DATE_EPOCH commit timestamp (validated when set)
#   DEBIAN_SNAPSHOT pinned Debian apt snapshot override
#   NO_CACHE        set to 1 to disable Docker/BuildKit layer cache
#
# Output:
#   $OUT_DIR/tempo-zone
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

VERSION="${VERSION:-dev}"
OUT_DIR="${OUT_DIR:-./out}"
DEBIAN_SNAPSHOT="${DEBIAN_SNAPSHOT:-}"
NO_CACHE="${NO_CACHE:-0}"
CHECKED_OUT_COMMIT="$(git rev-parse HEAD)"
CHECKED_OUT_EPOCH="$(git log -1 --pretty=%ct)"
COMMIT="${GIT_SHA:-$CHECKED_OUT_COMMIT}"
SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$CHECKED_OUT_EPOCH}"

[[ "$NO_CACHE" == "0" || "$NO_CACHE" == "1" ]] || {
  echo "NO_CACHE must be 0 or 1" >&2
  exit 1
}

[[ "$COMMIT" =~ ^[0-9a-f]{40}$ && "$COMMIT" == "$CHECKED_OUT_COMMIT" ]] || {
  echo "GIT_SHA must be the full SHA of the checked-out commit" >&2
  exit 1
}
[[ "$SOURCE_DATE_EPOCH" =~ ^[0-9]+$ && "$SOURCE_DATE_EPOCH" == "$CHECKED_OUT_EPOCH" ]] || {
  echo "SOURCE_DATE_EPOCH must be the checked-out commit timestamp" >&2
  exit 1
}

echo "::group::Reproducible build inputs"
printf '  commit              = %s\n' "$COMMIT"
printf '  version             = %s\n' "$VERSION"
printf '  SOURCE_DATE_EPOCH   = %s\n' "$SOURCE_DATE_EPOCH"
printf '  Dockerfile          = docker/Dockerfile.reproducible\n'
printf '  out_dir             = %s\n' "$OUT_DIR"
[[ -n "$DEBIAN_SNAPSHOT" ]] && printf '  DEBIAN_SNAPSHOT     = %s (override)\n' "$DEBIAN_SNAPSHOT"
[[ "$NO_CACHE" == "1" ]] && printf '  cache               = disabled\n'
echo "::endgroup::"

mkdir -p "$OUT_DIR"

build_args=(
  --build-arg "SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH"
  --build-arg "GIT_SHA=$COMMIT"
  --build-arg "VERSION=$VERSION"
)
if [[ -n "$DEBIAN_SNAPSHOT" ]]; then
  build_args+=( --build-arg "DEBIAN_SNAPSHOT=$DEBIAN_SNAPSHOT" )
fi

build_options=()
if [[ "$NO_CACHE" == "1" ]]; then
  build_options+=( --no-cache )
fi

docker build \
  --platform linux/amd64 \
  "${build_options[@]}" \
  "${build_args[@]}" \
  -f docker/Dockerfile.reproducible \
  --target artifacts \
  --output "type=local,dest=$OUT_DIR" \
  .

echo "Reproducible binary written to $OUT_DIR/tempo-zone"
sha256sum "$OUT_DIR/tempo-zone"
