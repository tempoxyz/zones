import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("spf_range", Path(__file__).parents[1] / "validate-spf-range.py")
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


class SpfRange(unittest.TestCase):
    def test_every_intersecting_batch_is_validated(self):
        requested = []
        def generate(block, path):
            requested.append(block)
            numbers = [1, 2, 3] if block == 2 else [4, 5, 6]
            path.write_text(json.dumps({"zoneBlocks": [{"number": n} for n in numbers]}))
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "coverage.json"
            result = m.validate_range(2, 5, path, generate)
            self.assertEqual(requested, [2, 4])
            self.assertEqual([(b["from_block"], b["to_block"]) for b in result["batches"]], [(1, 3), (4, 6)])
            self.assertEqual(json.loads(path.read_text())["status"], "complete")
            self.assertTrue(all(len(b["witness_sha256"]) == 64 for b in result["batches"]))

    def test_gaps_overlap_and_missing_target_fail(self):
        for ranges in ([[1, 2], [4, 5]], [[1, 2], [2, 3]], [[2, 4]], [[]], [[3, 4]]):
            with self.subTest(ranges=ranges), tempfile.TemporaryDirectory() as directory:
                batches = iter(ranges)
                def generate(block, path):
                    path.write_text(json.dumps({"zoneBlocks": [{"number": n} for n in next(batches)]}))
                path = Path(directory) / "coverage.json"
                with self.assertRaises(ValueError):
                    m.validate_range(2, 5, path, generate)
                self.assertFalse(path.exists())

    def test_prover_failure_cannot_produce_complete_coverage(self):
        def generate(block, path):
            raise subprocess.CalledProcessError(1, ["prover"])
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "coverage.json"
            with self.assertRaises(subprocess.CalledProcessError):
                m.validate_range(2, 5, path, generate)
            self.assertFalse(path.exists())

    def test_waits_for_last_measured_block_to_be_submitted(self):
        with patch.object(m.subprocess, "check_output", side_effect=["hash", "4\n", "hash", "6\n"]), patch.object(m.time, "sleep") as sleep:
            m.wait_for_submission(5, "portal", "tempo", "zone", 120)
            sleep.assert_called_once_with(1)
            m.subprocess.check_output.assert_any_call(
                ["cast", "block", "hash", "--field", "number", "--rpc-url", "zone"],
                text=True, timeout=30)

    def test_unsubmitted_range_times_out(self):
        with patch.object(m.subprocess, "check_output", side_effect=["hash", "4\n"]), patch.object(m.time, "monotonic", side_effect=[0, 121]):
            with self.assertRaises(TimeoutError):
                m.wait_for_submission(5, "portal", "tempo", "zone", 120)


if __name__ == '__main__':
    unittest.main()
