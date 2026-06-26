#!/usr/bin/env bash
# Reproducible-build wrapper. Single source of truth for producing the
# byte-deterministic `tempo-zone` binary for x86_64-unknown-linux-gnu.
#
# Inputs (env):
#   VERSION         informational version baked into audit output (default: dev)
#   OUT_DIR         where the built binary lands (default: ./out)
#   DEBIAN_SNAPSHOT pinned Debian apt snapshot override
#
# Output:
#   $OUT_DIR/tempo-zone
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

VERSION="${VERSION:-dev}"
OUT_DIR="${OUT_DIR:-./out}"
DEBIAN_SNAPSHOT="${DEBIAN_SNAPSHOT:-}"

SOURCE_DATE_EPOCH="$(git log -1 --pretty=%ct)"
COMMIT="$(git rev-parse HEAD)"

echo "::group::Reproducible build inputs"
printf '  commit              = %s\n' "$COMMIT"
printf '  version             = %s\n' "$VERSION"
printf '  SOURCE_DATE_EPOCH   = %s\n' "$SOURCE_DATE_EPOCH"
printf '  Dockerfile          = Dockerfile.reproducible\n'
printf '  out_dir             = %s\n' "$OUT_DIR"
[[ -n "$DEBIAN_SNAPSHOT" ]] && printf '  DEBIAN_SNAPSHOT     = %s (override)\n' "$DEBIAN_SNAPSHOT"
echo "::endgroup::"

mkdir -p "$OUT_DIR"

build_args=(
  --build-arg "SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH"
  --build-arg "VERSION=$VERSION"
)
if [[ -n "$DEBIAN_SNAPSHOT" ]]; then
  build_args+=( --build-arg "DEBIAN_SNAPSHOT=$DEBIAN_SNAPSHOT" )
fi

docker build \
  --platform linux/amd64 \
  "${build_args[@]}" \
  -f Dockerfile.reproducible \
  --target artifacts \
  --output "type=local,dest=$OUT_DIR" \
  .

echo "Reproducible binary written to $OUT_DIR/tempo-zone"
sha256sum "$OUT_DIR/tempo-zone"
