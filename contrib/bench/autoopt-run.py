#!/usr/bin/env python3
"""Build both revisions and collect paired live journeys on the dedicated runner."""
from __future__ import annotations

import hashlib
import json
import os
import re
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


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def save(manifest):
    temporary = OUTPUT / "experiment.json.tmp"
    temporary.write_text(json.dumps(manifest, indent=2, allow_nan=False) + "\n")
    temporary.replace(OUTPUT / "experiment.json")


def main():
    req = json.loads(os.environ["ZONES_AUTOOPT_REQUEST"])
    if req != request(req["baseline_sha"], req["candidate_sha"], req["workload"], req.get("attempt_id")):
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
                "runs": [], "profiles": [], "builds": {}, "status": "building",
                "resolved": {k: v for k, v in os.environ.items() if k.startswith("ZONES_BENCH_") and
                             k.removeprefix("ZONES_BENCH_") in {
                                 "TEMPO_REF", "TXGEN_REF", "EARN_REVISION", "SAMPLY_REF", "TEMPO_HARDFORK", "L1_CACHE_KEY",
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
            if side == "candidate" and sha == req["baseline_sha"]:
                binaries[side] = binaries["baseline"]
                for suffix in ("build.log", "tests.log"):
                    shutil.copy2(OUTPUT / f"baseline-{suffix}", OUTPUT / f"candidate-{suffix}")
                manifest["builds"][side] = dict(manifest["builds"]["baseline"], reused_from="baseline")
                save(manifest)
                continue
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
                run(["cargo", "test", "--locked", "--target-dir", os.environ["ZONES_BENCH_ZONES_TARGET_DIR"], "-p", "zone-sequencer", "-p", "zone-evm"],
                    cwd=checkout, stdout=log, stderr=subprocess.STDOUT)
            test_output = re.sub(r"\x1b\[[0-9;]*m", "", (OUTPUT / f"{side}-tests.log").read_text())
            if not re.search(r"test result: ok\. [1-9][0-9]* passed", test_output):
                raise ValueError(f"{side} correctness command ran no passing tests")
            manifest["builds"][side] = {"sha": sha, "binaries": {
                name: digest(destination / name) for name in ("tempo-zone", "tempo-zone-prover-utils")},
                "test_log_sha256": digest(OUTPUT / f"{side}-tests.log")}
            save(manifest)
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
                       ZONES_BENCH_METRICS_BEFORE_FILE=str(directory / "zone-before.prom"),
                       ZONES_BENCH_METRICS_AFTER_FILE=str(directory / "zone-after.prom"),
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
                     "report": json.loads((directory / "report.json").read_text()), "directory": label,
                     "report_sha256": digest(directory / "report.json"),
                     "validated_spf_sha256": digest(directory / "spf-input.json")}
            if profiled:
                entry["profile_sha256"] = digest(directory / "zone-profile.json.gz")
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
