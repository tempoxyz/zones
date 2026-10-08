#!/usr/bin/env python3
"""Validate and evaluate paired Zones journey reports. No GitHub side effects."""
from __future__ import annotations

import argparse
import hashlib
import json
import math
from pathlib import Path
import re
import statistics

SHA = re.compile(r"[0-9a-f]{40}")
POLICY = {"pairs": 6, "minimum_improvement": 0.02, "maximum_p99_regression": 0.05,
          "primary_metric": "client_observed_e2e_latency.mean_ms", "alpha": 0.05}


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), allow_nan=False)


def request(baseline, candidate, workload, attempt_id=None):
    for revision in (baseline, candidate):
        if not isinstance(revision, str) or not SHA.fullmatch(revision):
            raise ValueError("revisions must be full lowercase commit SHAs")
    if not isinstance(workload, dict) or not workload:
        raise ValueError("workload must be a nonempty mapping of resolved settings")
    body = {"version": 1, "baseline_sha": baseline, "candidate_sha": candidate,
            "workload": workload, "policy": POLICY.copy()}
    if attempt_id is not None:
        if not isinstance(attempt_id, str) or not re.fullmatch(r"[0-9a-f]{32}", attempt_id):
            raise ValueError("attempt_id must be a UUID hex string")
        body["attempt_id"] = attempt_id
    return dict(body, experiment_id=hashlib.sha256(canonical(body).encode()).hexdigest())


def positive(value, label):
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value <= 0:
        raise ValueError(f"{label} must be finite and positive")
    return value


def measurement(report, count):
    if report.get("version") != 2:
        raise ValueError("requires scenario report version 2")
    for key in ("started", "completed"):
        if type(report.get(key)) is not int or report[key] != count:
            raise ValueError(f"{key} does not match requested journey count")
    for key in ("failed", "timed_out"):
        if type(report.get(key)) is not int or report[key] != 0:
            raise ValueError(f"{key} is not zero")
    latency = report["client_observed_e2e_latency"]
    if latency["samples"] != count:
        raise ValueError("latency sample count does not match completed journeys")
    return (positive(latency["mean_ms"], "mean latency"),
            positive(latency["p99_ms"], "p99 latency"))


def evaluate(req, runs):
    expected = request(req["baseline_sha"], req["candidate_sha"], req["workload"], req.get("attempt_id"))
    if req != expected:
        raise ValueError("request identity or policy mismatch")
    count = req["workload"]["count"]
    if type(count) is not int or count <= 0:
        raise ValueError("workload count must be a positive integer")
    if len(runs) != 2 * POLICY["pairs"]:
        raise ValueError("requires all six complete pairs")
    values = {"baseline": [], "candidate": []}
    tails = {"baseline": [], "candidate": []}
    for pair in range(POLICY["pairs"]):
        order = ("baseline", "candidate") if pair % 2 == 0 else ("candidate", "baseline")
        for offset, side in enumerate(order):
            run = runs[2 * pair + offset]
            if (run["pair"] != pair or run["side"] != side or
                    run["sha"] != req[f"{side}_sha"] or run.get("profiled") is not False):
                raise ValueError("run order, commit, or profiling mismatch")
            mean, p99 = measurement(run["report"], count)
            values[side].append(mean)
            tails[side].append(p99)
    improvements = [1 - b / a for a, b in zip(values["baseline"], values["candidate"])]
    # Exact two-sided paired sign test; ties carry no evidence.
    nonzero = [x for x in improvements if x != 0]
    n = len(nonzero)
    k = min(sum(x > 0 for x in nonzero), sum(x < 0 for x in nonzero))
    p_value = min(1.0, 2 * sum(math.comb(n, i) for i in range(k + 1)) / 2 ** n)
    median = statistics.median(improvements)
    tail_regression = max(b / a - 1 for a, b in zip(tails["baseline"], tails["candidate"]))
    verdict = "no_measurable_improvement"
    tail_violated = any(b > a * (1 + POLICY["maximum_p99_regression"])
                        for a, b in zip(tails["baseline"], tails["candidate"]))
    significant = p_value <= POLICY["alpha"]
    control_tail_changed = any(b < a * (1 - POLICY["maximum_p99_regression"]) or b > a * (1 + POLICY["maximum_p99_regression"])
                               for a, b in zip(tails["baseline"], tails["candidate"]))
    if req["baseline_sha"] == req["candidate_sha"] and (
        control_tail_changed or (abs(median) >= POLICY["minimum_improvement"] and significant)
    ):
        verdict = "unstable_control"
    elif tail_violated:
        verdict = "regression"
    elif median >= POLICY["minimum_improvement"] and significant:
        verdict = "performance_candidate"
    return {"version": 1, "experiment_id": req["experiment_id"], "verdict": verdict,
            "paired_improvements": improvements, "median_improvement": median,
            "p_value": p_value, "maximum_p99_regression": tail_regression,
            "latency_ms": {side: {"median": statistics.median(xs), "min": min(xs), "max": max(xs)}
                           for side, xs in values.items()}}


def render_summary(req, result, runs):
    lines = ["# Zones experiment", "", f"Result: **{result['verdict']}**", "",
             "A performance candidate requires correctness and causal review before acceptance.", "",
             f"Baseline: `{req['baseline_sha']}`", f"Candidate: `{req['candidate_sha']}`", "",
             f"Median paired mean-latency improvement: {result['median_improvement'] * 100:.2f}%.",
             f"Paired sign-test p-value: {result['p_value']:.5f}.", "",
             "| Pair | Baseline mean (ms) | Candidate mean (ms) | Baseline p99 (ms) | Candidate p99 (ms) |",
             "| --- | ---: | ---: | ---: | ---: |"]
    for pair in range(POLICY["pairs"]):
        sides = {r["side"]: r["report"]["client_observed_e2e_latency"] for r in runs if r["pair"] == pair}
        a, b = sides["baseline"], sides["candidate"]
        lines.append(f"| {pair + 1} | {a['mean_ms']:.3f} | {b['mean_ms']:.3f} | {a['p99_ms']:.3f} | {b['p99_ms']:.3f} |")
    lines += ["", "Every measured report completed all requested journeys with zero failures and timeouts.",
              "Profile runs are separate and excluded from this table.", "", "## Workload", "", "```json",
              json.dumps(req["workload"], indent=2), "```", "", "## Evidence review", "",
              "Download this run's `zones-autoopt` artifact. Inspect `experiment.json`, both `profile-0-*` directories,",
              "the raw journey reports and causal events, metrics snapshots, node logs, and correctness logs.",
              "Establish the limiter and map the changed frame or wait to the source diff before accepting a gain."]
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    manifest = json.loads(args.manifest.read_text())
    result = evaluate(manifest["request"], manifest["runs"])
    args.output.write_text(json.dumps(result, indent=2, allow_nan=False) + "\n")


if __name__ == "__main__":
    main()
