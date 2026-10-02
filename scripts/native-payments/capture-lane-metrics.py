#!/usr/bin/env python3
"""Sample Tempo's per-payload lane counters while a devnet workload runs."""
import argparse
import json
import re
import time
import urllib.request
from pathlib import Path

NAMES = (
    "payment_transactions_last",
    "general_gas_used_last",
    "payment_gas_used_last",
    "gas_used_last",
    "pool_transactions_included_last",
)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--metrics-url", required=True)
    parser.add_argument("--seconds", type=float, default=90)
    parser.add_argument("--interval", type=float, default=0.1)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.seconds <= 0 or args.interval <= 0:
        parser.error("seconds and interval must be positive")

    deadline = time.monotonic() + args.seconds
    with args.output.open("w") as output:
        while time.monotonic() < deadline:
            try:
                body = urllib.request.urlopen(args.metrics_url, timeout=2).read().decode()
                values = {}
                for name in NAMES:
                    match = re.search(
                        r"^reth_tempo_payload_builder_" + name + r" ([0-9.e+-]+)$",
                        body, re.MULTILINE,
                    )
                    if match is None:
                        raise ValueError(f"missing payload metric {name}")
                    values[name] = float(match.group(1))
                if values["pool_transactions_included_last"]:
                    output.write(json.dumps({"time": time.time(), **values}) + "\n")
                    output.flush()
            except (OSError, ValueError) as error:
                print(f"metrics sample failed: {error}", flush=True)
            time.sleep(args.interval)


if __name__ == "__main__":
    main()
