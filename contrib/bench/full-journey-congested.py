#!/usr/bin/env python3
"""A fixed congestion schedule around the existing txgen full journey."""

import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
from urllib.request import Request, urlopen


# Keep these controls out of workflow_dispatch (already at its input limit).
UNCONGESTED_SECS = 60
CONGESTED_SECS = 60
EXPIRY_SECS = 5  # Must match l1-congestion.yml.
RECOVERY_SECS = 60
BACKGROUND_TPS = 1000
BACKGROUND_GAS = 2_000_000  # Also the encrypted onramp's gas reservation.
MIN_CONGESTED_FRACTION = 0.8
MAX_RUN_SECS = 1200
SPEC = Path(__file__).parent / "neobank/l1-congestion.yml"


def quantity(value):
    return int(value, 16) if isinstance(value, str) else int(value)


def settings(env):
    count = int(env["ZONES_BENCH_COUNT"])
    requested_rate = float(env["ZONES_BENCH_TPS"])
    if not 20 <= count <= 10_000:
        raise ValueError("full-journey-congested requires count between 20 and 10000")
    if requested_rate < 0.1:
        raise ValueError("full-journey-congested requires tps >= 0.1")
    if int(env["ZONES_BENCH_ACCOUNT_START"]) <= 5:
        raise ValueError("congestion reserves funded mnemonic index 5; users must start after it")
    match = re.fullmatch(r"([1-9][0-9]*)(ms|s|m|h)", env["ZONES_BENCH_STEP_TIMEOUT"])
    if not match:
        raise ValueError("invalid step-timeout")
    timeout = int(match[1]) * {"ms": 0.001, "s": 1, "m": 60, "h": 3600}[match[2]]
    if not 180 <= timeout <= 600:
        raise ValueError("full-journey-congested requires step-timeout between 3m and 10m")
    window = UNCONGESTED_SECS + CONGESTED_SECS + EXPIRY_SECS + RECOVERY_SECS
    # Respect the requested rate as a ceiling. In-flight backpressure can delay
    # starts further, but cannot spend the entire count before recovery.
    rate = min(requested_rate, (count - 1) / window)
    if (count - 1) / rate + 600 > MAX_RUN_SECS:
        raise ValueError("count/tps leaves less than 10m to finish within the 20m run limit")
    return count, rate


def rpc(url, method, params):
    body = json.dumps(dict(jsonrpc="2.0", id=1, method=method, params=params)).encode()
    with urlopen(Request(url, body, {"Content-Type": "application/json"}), timeout=5) as response:
        result = json.load(response)
    if "error" in result:
        raise RuntimeError(f"{method}: {result['error']}")
    if result.get("result") is None:
        raise RuntimeError(f"{method}: missing result")
    return result["result"]


def block_record(block, receipts, sender, expected_limit):
    """Bound background capacity gas without confusing receipt/state gas.

    Header gas is actual block capacity gas. Every other transaction's declared
    gas limit is an upper bound on its contribution, including payments and
    subblocks. Subtracting those limits gives a lower bound on background gas.
    In this topology the payload builder puts these non-payment calls in the
    main block. System transactions have zero gas limit and consume no capacity.
    """
    transactions = block["transactions"]
    by_hash = {receipt["transactionHash"]: receipt for receipt in receipts}
    if len(by_hash) != len(transactions):
        raise ValueError("incomplete block receipts")
    limit = quantity(block["mainBlockGeneralGasLimit"])
    if limit != expected_limit:
        raise ValueError(f"included general gas limit {limit} != configured {expected_limit}")
    usable = min(limit, quantity(block["gasLimit"]) - quantity(block["sharedGasLimit"]))
    background = []
    other_limit = 0
    for tx in transactions:
        receipt = by_hash[tx["hash"]]
        if receipt["blockHash"] != block["hash"]:
            raise ValueError("receipt/block hash mismatch")
        if tx["from"].lower() == sender:
            calls = tx.get("calls") or [tx]
            if (len(calls) != 1 or calls[0].get("to", "").lower() != "0x" + "0" * 39 + "5"
                    or quantity(tx["gas"]) != BACKGROUND_GAS or quantity(receipt["status"]) != 0):
                raise ValueError("unexpected background transaction or successful modexp call")
            background.append({"hash": tx["hash"], "gas_used": quantity(receipt["gasUsed"])})
        else:
            other_limit += quantity(tx["gas"])
    used = quantity(block["gasUsed"])
    lower = max(0, used - other_limit) if background else 0
    if lower > usable or lower > len(background) * BACKGROUND_GAS:
        raise ValueError("background gas exceeds main-block capacity; unsupported accounting")
    return dict(
        block=quantity(block["number"]), block_hash=block["hash"],
        timestamp_ms=quantity(block["timestamp"]) * 1000 + quantity(block["timestampMillisPart"]),
        general_gas_limit=limit, usable_general_gas_limit=usable, block_gas_used=used,
        background=background, background_receipt_gas=sum(tx["gas_used"] for tx in background),
        other_transaction_gas_limit=other_limit, background_capacity_gas_lower_bound=lower,
        remaining_general_gas_upper_bound=usable - lower,
    )


def stop_processes(processes):
    # Separate sessions cover the whole generator/sender tree, including children.
    for process in processes:
        if process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
    deadline = time.monotonic() + 3
    for process in processes:
        try:
            process.wait(timeout=max(0.01, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()


def observe(url, first, sender, limit, output, stop, errors):
    try:
        previous_hash = None
        with output.open("w") as stream:
            while True:
                tip = quantity(rpc(url, "eth_blockNumber", []))
                while first <= tip:
                    if stop.is_set():
                        return
                    block = rpc(url, "eth_getBlockByNumber", [hex(first), True])
                    if previous_hash is not None and block["parentHash"] != previous_hash:
                        raise ValueError("L1 reorg during measurement")
                    receipts = rpc(url, "eth_getBlockReceipts", [hex(first)])
                    record = block_record(block, receipts, sender, limit)
                    stream.write(json.dumps(record) + "\n")
                    stream.flush()
                    previous_hash = block["hash"]
                    first += 1
                if stop.wait(0.25):
                    return
    except Exception as error:
        errors.append(str(error))


def summarize(output, report_path, schedule, count, rate, errors):
    records_path = output / "blocks.jsonl"
    blocks = [json.loads(line) for line in records_path.read_text().splitlines()] if records_path.exists() else []
    report = json.loads(report_path.read_text()) if report_path.exists() else {}
    instances = report.get("sampled_instances", [])
    if not blocks or blocks[-1]["timestamp_ms"] < schedule["finish_ms"] - 5000:
        errors.append("included-block observation did not cover the end of measurement")
    phases = []
    boundaries = [("uncongested", schedule["start_ms"], schedule.get("congestion_ms")),
                  ("congested", schedule.get("congestion_ms"), schedule.get("stop_ms")),
                  ("expiry", schedule.get("stop_ms"), schedule.get("recovery_ms")),
                  ("recovery", schedule.get("recovery_ms"), schedule["finish_ms"])]
    for name, start, end in boundaries:
        if start is None or end is None:
            continue
        selected = [b for b in blocks if start <= b["timestamp_ms"] < end]
        # Exclude the first five seconds of sender startup from the saturation gate.
        steady = [b for b in selected if b["timestamp_ms"] >= start + EXPIRY_SECS * 1000]
        saturated = [b for b in steady if b["background"] and
                     b["remaining_general_gas_upper_bound"] <= BACKGROUND_GAS]
        completed = [i for i in instances if i["outcome"] == "completed" and
                     start <= i["finished_at_unix_ms"] < end]
        phases.append(dict(
            phase=name, start_ms=start, end_ms=end, blocks=len(selected),
            included_background_transactions=sum(len(b["background"]) for b in selected),
            included_background_receipt_gas=sum(b["background_receipt_gas"] for b in selected),
            remaining_gas_upper_bound_max=max((b["remaining_general_gas_upper_bound"] for b in steady), default=None),
            saturated_blocks=len(saturated), steady_blocks=len(steady),
            starts=sum(start <= i["started_at_unix_ms"] < end for i in instances),
            completions=len(completed),
            failures=sum(i["outcome"] != "completed" and start <= i["finished_at_unix_ms"] < end for i in instances),
            in_flight_at_end=sum(i["started_at_unix_ms"] < end <= i["finished_at_unix_ms"] for i in instances),
        ))
    by_phase = {phase["phase"]: phase for phase in phases}
    congested = by_phase.get("congested", {})
    recovery = by_phase.get("recovery", {})
    if not congested.get("steady_blocks") or congested.get("saturated_blocks", 0) < MIN_CONGESTED_FRACTION * congested.get("steady_blocks", 0):
        errors.append("background did not leave <= 2000000 gas in at least 80% of steady congested blocks")
    if not by_phase.get("uncongested", {}).get("completions"):
        errors.append("no uncongested journey completion")
    if not recovery.get("completions") or not recovery.get("starts"):
        errors.append("recovery needs both new starts and completed journeys")
    if not any(i["outcome"] == "completed" and i["started_at_unix_ms"] >= schedule.get("recovery_ms", schedule["finish_ms"])
               for i in instances):
        errors.append("no journey started after recovery completed")
    if not any(i["started_at_unix_ms"] < congested.get("end_ms", 0) and
               i["finished_at_unix_ms"] > congested.get("start_ms", schedule["finish_ms"]) + EXPIRY_SECS * 1000
               for i in instances):
        errors.append("no journey overlapped steady congestion")
    if by_phase.get("uncongested", {}).get("included_background_transactions", 0):
        errors.append("background transactions were included during the uncongested phase")
    if recovery.get("included_background_transactions", 0):
        errors.append("background transactions were included after the expiry window")
    if not recovery.get("blocks"):
        errors.append("no included recovery blocks")
    if report.get("started") != count or len(instances) != count:
        errors.append("missing journey outcomes; full lifecycle sampling is required")
    if report.get("completed", 0) + report.get("failed", 0) != count:
        errors.append("journeys did not reach terminal outcomes")
    recovery_start = schedule.get("recovery_ms", schedule["finish_ms"])
    rescued = [i for i in instances if i["outcome"] == "completed" and
               i["started_at_unix_ms"] < recovery_start <= i["finished_at_unix_ms"]]
    first_recovery = min((i["finished_at_unix_ms"] - recovery_start for i in instances
                          if i["outcome"] == "completed" and i["finished_at_unix_ms"] >= recovery_start), default=None)
    result = dict(schedule=schedule, effective_starts_per_second=rate, phases=phases,
                  pending_journeys_completed_after_recovery=len(rescued),
                  first_recovery_completion_ms=first_recovery, errors=errors)
    (output / "report.json").write_text(json.dumps(result, indent=2) + "\n")
    lines = ["\n## L1 congestion\n", f"Effective journey starts/s: {rate:.6g}; requested count: {count}.\n",
             "| Phase | Blocks | Background txs | Receipt gas | Saturated blocks¹ | Starts | Completed | Failed | In flight at end |",
             "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"]
    for p in phases:
        lines.append(f"| {p['phase']} | {p['blocks']} | {p['included_background_transactions']} | "
                     f"{p['included_background_receipt_gas']} | {p['saturated_blocks']}/{p['steady_blocks']} | "
                     f"{p['starts']} | {p['completions']} | {p['failures']} | {p['in_flight_at_end']} |")
    lines += ["\n¹ Remaining usable general gas **upper bound ≤ 2,000,000**, after five seconds of phase startup. "
              "Capacity is bounded from included block gas minus other transactions’ gas limits; receipt gas is reported separately. "
              "Per-block bounds and transaction hashes are in `congestion/blocks.jsonl`.\n",
              f"Pending journeys completed after recovery: {len(rescued)}. "
              f"Time to first recovery completion: {first_recovery if first_recovery is not None else 'unavailable'} ms. "
              "Existing journey/step reports and all sampled lifecycles contain the latency details.\n"]
    lines += [f"- {error}" for error in errors]
    (output / "summary.md").write_text("\n".join(lines) + "\n")


def run(command):
    env = os.environ
    count, rate = settings(env)
    output = Path(env["ZONES_BENCH_OUTPUT"]) / "congestion"
    output.mkdir(parents=True, exist_ok=True)
    txgen = command[0]
    bench = env.get("TXGEN_BENCH_BIN", "bench")
    if not shutil.which(bench):
        raise ValueError(f"missing {bench}")
    sender = subprocess.check_output([txgen, "addresses", "--spec", str(SPEC)], text=True).strip().lower()
    if not re.fullmatch(r"0x[0-9a-f]{40}", sender):
        raise ValueError("expected exactly one congestion account")
    url = env["ZONES_BENCH_L1_QUERY_RPC_URL"]
    head = rpc(url, "eth_getBlockByNumber", ["latest", True])
    limit = int(env["ZONES_BENCH_L1_GENERAL_GAS_LIMIT"])
    block_record(head, rpc(url, "eth_getBlockReceipts", [head["number"]]), sender, limit)
    if int(env["ZONES_BENCH_L1_MAX_FEE_PER_GAS"]) < quantity(head["baseFeePerGas"]) + 1_000_000_000:
        raise ValueError("L1 max fee must cover base fee plus the background's 1 gwei priority fee")
    # Validate generation before either the measurement clock or congestion starts.
    subprocess.run([txgen, "generate", "--spec", str(SPEC), "--count", "1", "--seed", env["ZONES_BENCH_SEED"]],
                   stdout=subprocess.DEVNULL, check=True)
    schedule = dict(start_ms=time.time_ns() // 1_000_000, uncongested_secs=UNCONGESTED_SECS,
                    congested_secs=CONGESTED_SECS, expiry_secs=EXPIRY_SECS,
                    recovery_secs=RECOVERY_SECS, background_tps=BACKGROUND_TPS,
                    background_gas_limit=BACKGROUND_GAS, background_sender=sender)
    processes, background, errors = [], [], []
    stop = threading.Event()
    observer = threading.Thread(target=observe, args=(url, quantity(head["number"]) + 1, sender, limit,
                                output / "blocks.jsonl", stop, errors), daemon=True)
    def cancelled(signum, _frame):
        raise InterruptedError(f"cancelled by signal {signum}")
    signal.signal(signal.SIGTERM, cancelled)
    signal.signal(signal.SIGINT, cancelled)
    started = time.monotonic()
    try:
        observer.start()
        measured = subprocess.Popen(command + ["--failure-policy", "continue", "--starts-per-second", str(rate),
                                    "--sample-instances", str(count)], start_new_session=True)
        processes.append(measured)
        with (output / "traffic.log").open("w") as log:
            while True:
                elapsed = time.monotonic() - started
                if errors:
                    raise RuntimeError("L1 observation failed")
                if elapsed >= MAX_RUN_SECS:
                    raise TimeoutError("congested benchmark exceeded 20 minutes")
                if elapsed >= UNCONGESTED_SECS and "congestion_ms" not in schedule:
                    schedule["congestion_ms"] = time.time_ns() // 1_000_000
                    generator = subprocess.Popen([txgen, "generate", "--spec", str(SPEC), "--duration", "1h",
                                                  "--seed", env["ZONES_BENCH_SEED"]],
                                                 stdout=subprocess.PIPE, stderr=log, start_new_session=True)
                    processes.append(generator)
                    background.append(generator)
                    sender_process = subprocess.Popen([bench, "send", "--rpc-url", env["L1_RPC_URL"],
                                                       "--tps", str(BACKGROUND_TPS), "--max-concurrent", "32",
                                                       "--retries", "0", "--timeout", "2s", "--drain-timeout", "0"],
                                                      stdin=generator.stdout, stdout=log, stderr=log, start_new_session=True)
                    generator.stdout.close()
                    processes.append(sender_process)
                    background.append(sender_process)
                if elapsed >= UNCONGESTED_SECS + CONGESTED_SECS and "stop_ms" not in schedule:
                    stop_processes(background)
                    schedule["stop_ms"] = time.time_ns() // 1_000_000
                    schedule["recovery_ms"] = schedule["stop_ms"] + (EXPIRY_SECS + 1) * 1000
                if "stop_ms" not in schedule and any(p.poll() is not None for p in background):
                    raise RuntimeError("background generator/sender exited before the scheduled stop")
                if measured.poll() is not None:
                    if measured.returncode:
                        raise RuntimeError(f"journey runner exited {measured.returncode}")
                    if "recovery_ms" not in schedule:
                        raise RuntimeError("journey runner finished before recovery")
                    if time.time_ns() // 1_000_000 >= schedule["recovery_ms"] + RECOVERY_SECS * 1000:
                        break
                time.sleep(0.1)
    except Exception as error:
        errors.append(str(error))
    finally:
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        stop_processes(processes)
        stop.set()
        observer.join(timeout=15)
        if observer.is_alive():
            errors.append("L1 observer did not finish")
        schedule["finish_ms"] = time.time_ns() // 1_000_000
        summarize(output, Path(env["ZONES_BENCH_REPORT"]), schedule, count, rate, errors)
    if errors:
        raise RuntimeError("; ".join(errors))


if __name__ == "__main__":
    try:
        if sys.argv[1:] == ["check"]:
            settings(os.environ)
        elif sys.argv[1:3] == ["run", "--"]:
            run(sys.argv[3:])
        else:
            raise ValueError("usage: full-journey-congested.py check | run -- TXGEN scenario run ...")
    except Exception as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(1)
