#!/usr/bin/env python3
"""Build both revisions and collect paired live journeys on the dedicated runner."""
from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

from autoopt_results import POLICY, canonical, evaluate, request

ROOT = Path(__file__).resolve().parents[2]
OUTPUT = ROOT / "target/zones-benchmark/autoopt"
# These determine the workload sent to the existing target-owned runner.
INPUTS = {"preset": "NEOBANK_PRESET", "accounts": "ACCOUNTS", "count": "COUNT", "tps": "TPS",
          "max-concurrent": "MAX_CONCURRENT", "state-bloat-gib": "BLOAT_GIB", "seed": "SEED",
          "txgen-ref": "TXGEN_REF"}


def run(args, **kwargs):
    return subprocess.run([str(x) for x in args], check=True, **kwargs)


def git(*args):
    return subprocess.check_output(["git", "-C", str(ROOT), *args], text=True).strip()


def save(manifest):
    temporary = OUTPUT / "experiment.json.tmp"
    temporary.write_text(json.dumps(manifest, indent=2, allow_nan=False) + "\n")
    temporary.replace(OUTPUT / "experiment.json")


def main():
    req = json.loads(os.environ["ZONES_AUTOOPT_REQUEST"])
    if req != request(req["baseline_sha"], req["candidate_sha"], req["workload"]):
        raise ValueError("invalid experiment identity or unsupported policy")
    if set(req["workload"]) != set(INPUTS):
        raise ValueError(f"workload must specify exactly {sorted(INPUTS)}")
    for key, suffix in INPUTS.items():
        if str(req["workload"][key]) != os.environ.get("ZONES_BENCH_" + suffix):
            raise ValueError(f"workflow resolved a different {key}")
    OUTPUT.mkdir(parents=True, exist_ok=False)
    manifest = {"version": 1, "request": req, "harness_sha": git("rev-parse", "HEAD"),
                "github_run_id": os.environ["GITHUB_RUN_ID"],
                "github_run_attempt": os.environ["GITHUB_RUN_ATTEMPT"],
                "runs": [], "profiles": [], "status": "building",
                "resolved": {k: v for k, v in os.environ.items() if k.startswith("ZONES_BENCH_") and
                             k.removeprefix("ZONES_BENCH_") in {
                                 "TEMPO_REF", "TXGEN_REF", "EARN_REVISION", "TEMPO_HARDFORK", "L1_CACHE_KEY",
                                 "L1_CACHE_GENERATION", "BUILD_PROFILE", "L1_A_CPUS", "L1_B_CPUS", "ZONE_CPUS", "CPUSET"}}}
    manifest["toolchain"] = subprocess.check_output(["rustc", "-vV"], text=True)
    manifest["rustflags"] = os.environ.get("RUSTFLAGS", "")
    manifest["host"] = {"cpus": os.cpu_count(), "load": os.getloadavg(),
                        "lscpu": subprocess.check_output(["lscpu"], text=True)}
    save(manifest)
    # Keep executables off the scratch volumes reset between measurements.
    scratch = Path(tempfile.mkdtemp(prefix="zones-autoopt-", dir=os.environ["RUNNER_TEMP"]))
    binaries = {}
    worktrees = []
    try:
        for side in ("baseline", "candidate"):
            sha = req[f"{side}_sha"]
            run(["git", "fetch", "--no-tags", "origin", sha], cwd=ROOT)
            checkout = scratch / side
            run(["git", "worktree", "add", "--detach", checkout, sha], cwd=ROOT)
            worktrees.append(checkout)
            # Both sides use the same L1, genesis, contracts and target-owned workload.
            # A change to those inputs requires a distinct benchmark protocol.
            for path in ("Cargo.lock", "Cargo.toml", "crates/contracts", "xtask", "rust-toolchain.toml"):
                if git("diff", "--name-only", manifest["harness_sha"], sha, "--", path):
                    raise ValueError(f"{side} changes shared benchmark input {path}")
            run(["git", "submodule", "update", "--init", "--recursive"], cwd=checkout)
            with (OUTPUT / f"{side}-build.log").open("w") as log:
                run(["cargo", "build", "--locked", "--profile", "profiling", "--target-dir",
                     os.environ["ZONES_BENCH_ZONES_TARGET_DIR"], "-p", "tempo-zone", "-p", "tempo-zone-prover-utils"],
                    cwd=checkout, stdout=log, stderr=subprocess.STDOUT)
            destination = scratch / f"{side}-bin"
            destination.mkdir()
            for name in ("tempo-zone", "tempo-zone-prover-utils"):
                shutil.copy2(Path(os.environ["ZONES_BENCH_ZONES_TARGET_DIR"]) / "profiling" / name, destination / name)
            binaries[side] = destination
            with (OUTPUT / f"{side}-tests.log").open("w") as log:
                run(["cargo", "test", "--locked", "-p", "zone-sequencer", "-p", "zone-evm"],
                    cwd=checkout, stdout=log, stderr=subprocess.STDOUT)
        manifest["status"] = "measuring"
        save(manifest)
        schedule = [(pair, side, False) for pair in range(POLICY["pairs"])
                    for side in (("baseline", "candidate") if pair % 2 == 0 else ("candidate", "baseline"))]
        schedule += [(0, side, True) for side in ("baseline", "candidate")]
        for pair, side, profiled in schedule:
            label = f'{"profile" if profiled else "pair"}-{pair}-{side}'
            directory = OUTPUT / label
            directory.mkdir()
            for volume, mount in (("a", "/reth-bench-a"), ("b", "/reth-bench-b")):
                tool = Path(os.environ["TEMPO_ROOT"]) / "bench-schelk.nu"
                config = f"/var/lib/schelk/{volume}.json"
                run(["nu", tool, "restore", config, mount])
                run(["nu", tool, "mark-dirty", config])
            env = dict(os.environ)
            env.update(ZONE_BIN=str(binaries[side] / "tempo-zone"),
                       SPF_BIN=str(binaries[side] / "tempo-zone-prover-utils"),
                       ZONES_BENCH_ZONES_REF=req[f"{side}_sha"],
                       ZONES_BENCH_OUTPUT=str(directory / "journeys"),
                       ZONES_BENCH_REPORT=str(directory / "report.json"),
                       ZONES_BENCH_TOPOLOGY_DIR=str(directory / "topology"),
                       ZONES_BENCH_ENV_FILE=str(directory / "topology.env"),
                       ZONES_BENCH_SPF_RANGE=str(directory / "spf-range.env"),
                       ZONES_BENCH_SPF_OUTPUT=str(directory / "spf-input.json"),
                       ZONES_BENCH_ZONE_STATE_ROOT=f"/reth-bench-a/autoopt-{os.environ['GITHUB_RUN_ID']}-{label}",
                       ZONES_BENCH_RUN_ID=f"{os.environ['GITHUB_RUN_ID']}-{label}")
            if profiled:
                env["ZONES_BENCH_LIVE_PROFILE"] = str(directory / "zone-profile.json.gz")
            with (directory / "run.log").open("w") as log:
                run([ROOT / "contrib/bench/autoopt-leg.sh"], env=env, cwd=ROOT, stdout=log, stderr=subprocess.STDOUT)
            entry = {"pair": pair, "side": side, "sha": req[f"{side}_sha"], "profiled": profiled,
                     "report": json.loads((directory / "report.json").read_text()), "directory": label}
            manifest["profiles" if profiled else "runs"].append(entry)
            save(manifest)
        manifest["result"] = evaluate(req, manifest["runs"])
        manifest["status"] = "complete"
        save(manifest)
    except BaseException:
        manifest["status"] = "failed"
        save(manifest)
        raise
    finally:
        for checkout in worktrees:
            subprocess.run(["git", "worktree", "remove", "--force", str(checkout)], cwd=ROOT, check=False)
        shutil.rmtree(scratch)


if __name__ == "__main__":
    main()
