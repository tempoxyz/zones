"""Run with uv run --with pyyaml python -m unittest discover -s .github/tests."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import yaml

WORKFLOW = yaml.safe_load(
    (Path(__file__).parents[1] / "workflows/docker.yml").read_text()
)
STEP = next(
    step for step in WORKFLOW["jobs"]["build-and-push"]["steps"]
    if step.get("name") == "Configure persistent intermediate layer cache"
)


def cache_config(event, *, repository="tempoxyz/zones", head="tempoxyz/zones", base="main"):
    context = {
        "repository": repository,
        "event_name": event,
        "event": {"pull_request": {"base": {"ref": base}, "head": {"repo": {"full_name": head}}}}
        if event == "pull_request" else {},
    }
    # These expressions use only property access, equality and boolean operators,
    # which have the same behavior in JS for these string/boolean fixtures.
    expressions = {key: value.strip()[3:-2] for key, value in STEP["env"].items()}
    evaluated = subprocess.run(
        ["node", "-e", """
const {context, expressions} = JSON.parse(require('node:fs').readFileSync(0, 'utf8'));
const result = Object.fromEntries(Object.entries(expressions).map(([key, expression]) =>
  [key, String(new Function('github', `return (${expression})`)(context))]));
process.stdout.write(JSON.stringify(result));
"""], input=json.dumps({"context": context, "expressions": expressions}),
        text=True, capture_output=True, check=True,
    )
    with tempfile.TemporaryDirectory() as directory:
        env = {**os.environ, **json.loads(evaluated.stdout),
               "REGISTRY": "ghcr.io/tempoxyz", "RUNNER_TEMP": directory}
        script = STEP["run"].split("<<'PYTHON'\n", 1)[1].rsplit("\nPYTHON", 1)[0]
        subprocess.run([sys.executable, "-c", script], env=env, check=True)
        return json.loads((Path(directory) / "zones-buildcache.json").read_text())["target"]


class DockerBuildCacheTests(unittest.TestCase):
    def test_pr_queue_and_devnet_cache_policy(self):
        cases = [
            ("pull_request", {}, True, "pr"),
            ("pull_request", {"head": "contributor/zones"}, True, None),
            ("pull_request", {"base": "release"}, True, None),
            ("pull_request", {"repository": "contributor/zones", "head": "contributor/zones"}, False, None),
            ("merge_group", {}, True, None),
            ("workflow_dispatch", {}, True, "main"),
            ("push", {}, False, "main"),
            ("schedule", {}, False, "main"),
        ]
        for event, context, read_pr, write in cases:
            with self.subTest(event=event, **context):
                targets = cache_config(event, **context)
                self.assertIn("prover-chef", targets)
                self.assertIn("tempo-zone-prover-enclave", targets)
                for target, config in targets.items():
                    ref = f"ghcr.io/tempoxyz/tempo-zone-buildcache:{target}"
                    expected = [f"type=registry,ref={ref}"]
                    if read_pr:
                        expected.append(f"type=registry,ref={ref}-pr-main")
                    self.assertEqual(config["cache-from"], expected)
                    if write is None:
                        self.assertNotIn("cache-to", config)
                    else:
                        suffix = "-pr-main" if write == "pr" else ""
                        self.assertEqual(config["cache-to"], [f"type=registry,ref={ref}{suffix},mode=max"])


if __name__ == "__main__":
    unittest.main()
