import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from check_sizes import PROTOCOL_RUNTIMES, REQUIRED, check_artifact


def artifact(runtime_size, initcode_size=1):
    return {
        "deployedBytecode": {"object": "0x" + "00" * runtime_size},
        "bytecode": {"object": "0x" + "00" * initcode_size},
    }


class SizeTests(unittest.TestCase):
    def test_protocol_runtimes_can_exceed_both_creation_limits(self):
        for source, name in PROTOCOL_RUNTIMES:
            self.assertIn("informational", check_artifact(source, name, artifact(24577, 49153)))

    def test_ordinary_creation_boundaries(self):
        source, name = "src/runtime/tempo/SwapAndDepositRouter.sol", "SwapAndDepositRouter"
        self.assertIn("CREATE", check_artifact(source, name, artifact(24576, 49152)))
        for runtime, initcode in [(24577, 1), (1, 49153)]:
            with self.assertRaisesRegex(ValueError, "size limit"):
                check_artifact(source, name, artifact(runtime, initcode))

    def test_contract_name_alone_does_not_exempt_size(self):
        with self.assertRaisesRegex(ValueError, "size limit"):
            check_artifact("src/runtime/other/ZonePortal.sol", "ZonePortal", artifact(24577))

    def test_invalid_or_empty_protocol_runtime_fails(self):
        source, name = next(iter(PROTOCOL_RUNTIMES))
        for code in ["0x", "0x__library__", "0x0"]:
            value = artifact(1)
            value["deployedBytecode"]["object"] = code
            with self.assertRaises(ValueError):
                check_artifact(source, name, value)
        for field in ["linkReferences", "immutableReferences"]:
            value = artifact(1)
            value["deployedBytecode"][field] = {"reference": [1]}
            with self.assertRaisesRegex(ValueError, "unresolved"):
                check_artifact(source, name, value)

    def test_report_ignores_build_info_and_test_contracts(self):
        with tempfile.TemporaryDirectory() as out:
            for source, name in REQUIRED | {("test/LargeTest.sol", "LargeTest")}:
                value = artifact(24577 if name != "SwapAndDepositRouter" else 1)
                value["metadata"] = {"settings": {"compilationTarget": {source: name}}}
                path = Path(out) / Path(source).name / f"{name}.json"
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(json.dumps(value))
            path = Path(out) / "build-info" / "build.json"
            path.parent.mkdir()
            path.write_text("{}")
            result = subprocess.run(
                [sys.executable, str(Path(__file__).with_name("check_sizes.py")), "--out", out],
                capture_output=True, text=True,
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("LargeTest", result.stdout)
        self.assertIn("runtime-sha256=", result.stdout)

    def test_missing_artifacts_fail_closed(self):
        with tempfile.TemporaryDirectory() as out:
            result = subprocess.run(
                [sys.executable, str(Path(__file__).with_name("check_sizes.py")), "--out", out],
                capture_output=True, text=True,
            )
        self.assertEqual(result.returncode, 1)
        self.assertIn("missing production artifact", result.stderr)


if __name__ == "__main__":
    unittest.main()
