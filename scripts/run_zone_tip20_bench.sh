#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

if [[ -f .env ]]; then
  set -a
  # shellcheck disable=SC1091
  source .env
  set +a
fi

: "${ZONE_TIP20_BENCH_BACKEND:=soft}"
: "${ZONE_TIP20_BENCH_COUNTS:=1,100,1000,10000}"
: "${ZONE_TIP20_BENCH_SKIP_SIMULATION:=true}"
: "${ZONE_TIP20_BENCH_POLL_MS:=5000}"

export ZONE_TIP20_BENCH_BACKEND
export ZONE_TIP20_BENCH_COUNTS
export ZONE_TIP20_BENCH_SKIP_SIMULATION
export ZONE_TIP20_BENCH_POLL_MS

echo "Running TIP-20 zone proof bench"
echo "  backend=$ZONE_TIP20_BENCH_BACKEND"
echo "  counts=$ZONE_TIP20_BENCH_COUNTS"

cargo test -p zone-prover-sp1-program --test tip20_e2e_bench -- --ignored --nocapture
