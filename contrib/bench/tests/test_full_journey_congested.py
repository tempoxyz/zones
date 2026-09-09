import copy
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location(
    "congestion", Path(__file__).parents[1] / "full-journey-congested.py"
)
C = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(C)
SENDER = "0x" + "11" * 20
OTHER = "0x" + "22" * 20


def block_fixture():
    block = dict(number="0x1", hash="0xblock", parentHash="0xparent", timestamp="0x1",
                 timestampMillisPart="0x0", gasLimit=30_000_000, gasUsed=29_000_000,
                 mainBlockGeneralGasLimit=30_000_000, sharedGasLimit=0,
                 baseFeePerGas=10_000_000_000,
                 transactions=[dict(hash=f"0xbackground{n}", **{"from": SENDER}, gas=2_000_000,
                                    calls=[dict(to="0x" + "0" * 39 + "5")]) for n in range(14)]
                              + [dict(hash="0xother", **{"from": OTHER}, gas=2_000_000)])
    receipts = [dict(transactionHash=t["hash"], blockHash=block["hash"], status="0x0",
                     gasUsed=2_000_000) for t in block["transactions"]]
    return block, receipts


class CongestionTests(unittest.TestCase):
    def test_pacing_preserves_recovery_and_requested_rate_ceiling(self):
        env = dict(ZONES_BENCH_COUNT="100", ZONES_BENCH_TPS="20",
                   ZONES_BENCH_ACCOUNT_START="16", ZONES_BENCH_STEP_TIMEOUT="10m")
        count, rate = C.settings(env)
        self.assertGreaterEqual((count - 1) / rate, 185)
        self.assertLessEqual(rate, 20)
        for key, value in [("ZONES_BENCH_COUNT", "1"), ("ZONES_BENCH_TPS", "0.01"),
                           ("ZONES_BENCH_STEP_TIMEOUT", "45s"), ("ZONES_BENCH_ACCOUNT_START", "5")]:
            with self.subTest(key=key), self.assertRaises(ValueError):
                C.settings(dict(env, **{key: value}))

    def test_capacity_uses_block_gas_and_other_limits_not_receipt_sum(self):
        block, receipts = block_fixture()
        record = C.block_record(block, receipts, SENDER, 30_000_000)
        self.assertEqual(record["background_receipt_gas"], 28_000_000)
        self.assertEqual(record["background_capacity_gas_lower_bound"], 27_000_000)
        self.assertEqual(record["remaining_general_gas_upper_bound"], 3_000_000)
        # Unrelated full blocks must never establish background congestion.
        record = C.block_record(block, receipts, OTHER + "00", 30_000_000)
        self.assertEqual(record["background_capacity_gas_lower_bound"], 0)
        self.assertEqual(record["remaining_general_gas_upper_bound"], 30_000_000)

    def test_shared_reservation_reduces_usable_capacity(self):
        block, receipts = block_fixture()
        block["sharedGasLimit"] = 2_000_000
        record = C.block_record(block, receipts, SENDER, 30_000_000)
        self.assertEqual(record["usable_general_gas_limit"], 28_000_000)
        self.assertEqual(record["remaining_general_gas_upper_bound"], 1_000_000)

    def test_incomplete_receipts_and_wrong_lane_are_rejected(self):
        block, receipts = block_fixture()
        with self.assertRaisesRegex(ValueError, "incomplete"):
            C.block_record(block, receipts[:1], SENDER, 30_000_000)
        receipts[0]["blockHash"] = "another block"
        with self.assertRaisesRegex(ValueError, "mismatch"):
            C.block_record(block, receipts, SENDER, 30_000_000)
        receipts[0]["blockHash"] = block["hash"]
        block["transactions"][0]["calls"][0]["to"] = "0x20c0" + "00" * 18
        with self.assertRaisesRegex(ValueError, "unexpected"):
            C.block_record(block, receipts, SENDER, 30_000_000)

    def experiment(self, directory):
        schedule = dict(start_ms=0, congestion_ms=60_000, stop_ms=120_000,
                        recovery_ms=126_000, finish_ms=200_000)
        instances = []
        for n in range(20):
            start, finish, outcome = (1000, 50_000, "completed") if n < 2 else (
                (70_000, 100_000, "failed") if n < 12 else (
                    (80_000, 150_000, "completed") if n < 14 else (130_000, 160_000, "completed")))
            instances.append(dict(started_at_unix_ms=start, finished_at_unix_ms=finish, outcome=outcome))
        report = dict(started=20, completed=10, failed=10, sampled_instances=instances)
        report_path = directory / "journeys.json"
        report_path.write_text(json.dumps(report))
        blocks = []
        for second in range(200):
            background = 60 <= second < 120
            blocks.append(dict(timestamp_ms=second * 1000, background=[{}] if background else [],
                               background_receipt_gas=30_000_000 if background else 0,
                               remaining_general_gas_upper_bound=0 if background else 30_000_000))
        return schedule, report_path, blocks

    def test_report_accepts_failures_but_requires_inclusion_and_recovery(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            schedule, report, blocks = self.experiment(directory)
            for condition in ["valid", "submitted_only", "no_recovery", "lingering_background"]:
                rows = copy.deepcopy(blocks)
                if condition == "submitted_only":
                    for row in rows:
                        row["background"] = []
                if condition == "no_recovery":
                    schedule = dict(schedule, recovery_ms=201_000)
                if condition == "lingering_background":
                    schedule["recovery_ms"] = 126_000
                    rows[-1]["background"] = [{}]
                (directory / "blocks.jsonl").write_text("".join(json.dumps(r) + "\n" for r in rows))
                errors = []
                C.summarize(directory, report, schedule, 20, 0.5, errors)
                with self.subTest(condition=condition):
                    self.assertEqual(bool(errors), condition != "valid", errors)

    def test_process_group_cleanup_handles_term_resistant_child(self):
        code = "import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); print('ready',flush=True); time.sleep(60)"
        process = subprocess.Popen([sys.executable, "-c", code], stdout=subprocess.PIPE,
                                   text=True, start_new_session=True)
        try:
            self.assertEqual(process.stdout.readline().strip(), "ready")
            C.stop_processes([process])
            self.assertEqual(process.returncode, -signal.SIGKILL)
        finally:
            C.stop_processes([process])
            process.stdout.close()

    def test_controller_success_failure_and_cancellation_stop_all_processes(self):
        # Real owned process groups, with only RPC and txgen behavior mocked.
        for outcome in ["success", "failure", "cancelled"]:
            with self.subTest(outcome=outcome), tempfile.TemporaryDirectory() as tmp:
                processes = []
                real_popen = subprocess.Popen
                def popen(_command, **kwargs):
                    delay = 0.5 if outcome == "success" and not processes else 60
                    process = real_popen([sys.executable, "-c", f"import time; time.sleep({delay})"], **kwargs)
                    processes.append(process)
                    return process
                def observe(_url, _first, _sender, _limit, output, stop, errors):
                    output.write_text("")
                    while len(processes) < 3 and not stop.wait(0.01):
                        pass
                    if outcome == "cancelled":
                        os.kill(os.getpid(), signal.SIGTERM)
                    elif outcome == "failure":
                        errors.append("simulated observation failure")
                    stop.wait(3)
                real_summarize = C.summarize
                def summarize(output, report_path, schedule, count, rate, errors):
                    if outcome == "success":
                        self.assertFalse(errors)
                        self.assertGreater(schedule["finish_ms"], schedule["recovery_ms"])
                        self.assertTrue(all(p.poll() is not None for p in processes))
                        (output / "report.json").write_text("{}")
                    else:
                        real_summarize(output, report_path, schedule, count, rate, errors)
                head, receipts = block_fixture()
                env = dict(ZONES_BENCH_OUTPUT=tmp, ZONES_BENCH_REPORT=str(Path(tmp) / "report.json"),
                           ZONES_BENCH_L1_QUERY_RPC_URL="http://unused", L1_RPC_URL="http://unused",
                           ZONES_BENCH_L1_GENERAL_GAS_LIMIT="30000000",
                           ZONES_BENCH_L1_MAX_FEE_PER_GAS="12000000000", ZONES_BENCH_SEED="42")
                handlers = {sig: signal.getsignal(sig) for sig in [signal.SIGTERM, signal.SIGINT]}
                try:
                    with patch.dict(os.environ, env), patch.object(C, "settings", return_value=(20, 0.5)), \
                         patch.object(C, "UNCONGESTED_SECS", 0), patch.object(C, "observe", side_effect=observe), \
                         patch.object(C, "CONGESTED_SECS", 0.2), patch.object(C, "EXPIRY_SECS", 0), \
                         patch.object(C, "RECOVERY_SECS", 0.1), patch.object(C, "summarize", side_effect=summarize), \
                         patch.object(C, "rpc", side_effect=lambda _u, method, _p: head if method == "eth_getBlockByNumber" else receipts), \
                         patch.object(C.shutil, "which", return_value="bench"), \
                         patch.object(C.subprocess, "check_output", return_value=SENDER), \
                         patch.object(C.subprocess, "run"), patch.object(C.subprocess, "Popen", side_effect=popen):
                        if outcome == "success":
                            C.run(["txgen", "scenario", "run"])
                        else:
                            with self.assertRaises(RuntimeError):
                                C.run(["txgen", "scenario", "run"])
                    self.assertEqual(len(processes), 3)
                    self.assertTrue(all(p.poll() is not None for p in processes))
                    self.assertTrue((Path(tmp) / "congestion/report.json").exists())
                finally:
                    C.stop_processes(processes)
                    for sig, handler in handlers.items():
                        signal.signal(sig, handler)


if __name__ == "__main__":
    unittest.main()
