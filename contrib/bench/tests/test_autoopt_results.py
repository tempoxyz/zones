import copy
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("autoopt_results", Path(__file__).parents[1] / "autoopt_results.py")
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


def fixture(candidate="b" * 40):
    req = m.request("a" * 40, candidate, {"count": 100, "seed": 42})
    runs = []
    for pair in range(6):
        for side in (("baseline", "candidate") if pair % 2 == 0 else ("candidate", "baseline")):
            mean = 100 if side == "baseline" else 90
            runs.append({"pair": pair, "side": side, "sha": req[f"{side}_sha"], "profiled": False,
                         "report": {"version": 2, "started": 100, "completed": 100, "failed": 0,
                                    "timed_out": 0, "client_observed_e2e_latency": {
                                        "samples": 100, "mean_ms": mean, "p99_ms": 150}}})
    return req, runs


class Results(unittest.TestCase):
    def test_consistent_gain_is_only_a_candidate(self):
        req, runs = fixture()
        result = m.evaluate(req, runs)
        self.assertEqual(result["verdict"], "performance_candidate")
        self.assertEqual(result["p_value"], 0.03125)
        self.assertAlmostEqual(result["median_improvement"], 0.1)

    def test_control_never_wins(self):
        req, runs = fixture("a" * 40)
        self.assertEqual(m.evaluate(req, runs)["verdict"], "unstable_control")

    def test_incomplete_or_invalid_work_is_rejected(self):
        for field, value in (("completed", 99), ("failed", 1), ("timed_out", 1), ("version", 1)):
            with self.subTest(field=field):
                req, runs = fixture()
                runs[0]["report"][field] = value
                with self.assertRaises(ValueError):
                    m.evaluate(req, runs)

    def test_tail_regression_overrides_mean_gain(self):
        req, runs = fixture()
        runs[1]["report"]["client_observed_e2e_latency"]["p99_ms"] = 170
        self.assertEqual(m.evaluate(req, runs)["verdict"], "regression")

    def test_rejects_profile_wrong_commit_order_or_missing_pair(self):
        req, original = fixture()
        mutations = [lambda r: r[0].update(profiled=True), lambda r: r[0].update(sha="c" * 40),
                     lambda r: r.reverse(), lambda r: r.pop()]
        for mutate in mutations:
            runs = copy.deepcopy(original)
            mutate(runs)
            with self.assertRaises(ValueError):
                m.evaluate(req, runs)

    def test_nan_is_not_evidence(self):
        req, runs = fixture()
        runs[0]["report"]["client_observed_e2e_latency"]["mean_ms"] = float("nan")
        with self.assertRaises(ValueError):
            m.evaluate(req, runs)

    def test_mixed_directions_are_inconclusive(self):
        req, runs = fixture()
        runs[1]["report"]["client_observed_e2e_latency"]["mean_ms"] = 101
        self.assertEqual(m.evaluate(req, runs)["verdict"], "no_measurable_improvement")

    def test_identity_includes_policy_and_workload(self):
        req, runs = fixture()
        req["workload"]["seed"] = 43
        with self.assertRaises(ValueError):
            m.evaluate(req, runs)


if __name__ == "__main__":
    unittest.main()
